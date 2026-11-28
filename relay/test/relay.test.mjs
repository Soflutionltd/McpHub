// Essai local du relais contre `pnpm dev` (wrangler dev, RELAY_HOST=relay.localhost).
// Joue le rôle d'un McpHub : se connecte en WebSocket, répond aux requêtes transmises.
import http from "node:http";
import { createHash, randomBytes } from "node:crypto";
import WebSocket from "ws";

const PORT = Number(process.env.RELAY_PORT ?? 8787);
// Identifiant dérivé du secret, comme McpHub (hub_id_for) et le relais (hubIdFor).
function hubIdFor(secret) {
  const digest = createHash("sha256").update(secret).digest();
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
const SECRET = `mhrelay_${randomBytes(32).toString("base64url")}`;
const HUB = hubIdFor(SECRET);

function check(name, ok, info = "") {
  console.log(`${ok ? "OK  " : "FAIL"} ${name}${ok ? "" : ` ${info}`}`);
  if (!ok) process.exit(1);
}

function encode(header, body) {
  const head = Buffer.from(JSON.stringify(header));
  const len = Buffer.alloc(4);
  len.writeUInt32BE(head.length);
  return Buffer.concat([len, head, body]);
}

function decode(frame) {
  const len = frame.readUInt32BE(0);
  return { header: JSON.parse(frame.subarray(4, 4 + len).toString()), body: frame.subarray(4 + len) };
}

function publicRequest(path, { method = "GET", body = "", host = `${HUB}.relay.localhost` } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: "127.0.0.1", port: PORT, path, method, headers: { Host: host, "Content-Type": "application/json" } },
      (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString() }));
      },
    );
    req.on("error", reject);
    req.end(body);
  });
}

function connectHub(secret) {
  return new Promise((resolve) => {
    const ws = new WebSocket(`ws://127.0.0.1:${PORT}/connect?hub=${HUB}`, {
      headers: { Host: "relay.localhost", Authorization: `Bearer ${secret}` },
    });
    ws.on("open", () => resolve({ ws, ok: true }));
    ws.on("unexpected-response", (_req, res) => resolve({ ws, ok: false, status: res.statusCode }));
    ws.on("error", () => resolve({ ws, ok: false }));
  });
}

const offline = await publicRequest("/mcp");
check("hub hors ligne -> 503", offline.status === 503, offline.status);

const hub = await connectHub(SECRET);
check("connexion du hub", hub.ok, hub.status);
const seen = [];
hub.ws.on("message", (data) => {
  const { header, body } = decode(Buffer.from(data));
  seen.push(header);
  const reply = JSON.stringify({ echo: body.toString(), path: header.path, query: header.query, host: header.headers.host, ip: header.headers["cf-connecting-ip"] ?? null });
  hub.ws.send(encode({ t: "res", id: header.id, status: 201, headers: [["Content-Type", "application/json"], ["Connection", "close"], ["X-Test", "1"]] }, Buffer.from(reply)));
});

const forwarded = await publicRequest("/mcp?x=1", { method: "POST", body: '{"jsonrpc":"2.0"}' });
const echo = JSON.parse(forwarded.body);
check("requete transmise et reponse rendue", forwarded.status === 201 && echo.echo === '{"jsonrpc":"2.0"}' && echo.path === "/mcp" && echo.query === "x=1", forwarded.body);
check("hote du hub transmis", echo.host === `${HUB}.relay.localhost`, echo.host);
check("en-tetes de la reponse gardes, Connection retire", forwarded.headers["x-test"] === "1" && forwarded.headers.connection !== "close");

const intruder = await connectHub(`mhrelay_${randomBytes(32).toString("base64url")}`);
check("autre secret refuse pour cet identifiant", !intruder.ok && intruder.status === 401, intruder.status);

const bigLogin = await publicRequest("/register", { method: "POST", body: "x".repeat(1024 * 1024 + 10) });
check("corps de plus de 1 Mo hors /mcp -> 413", bigLogin.status === 413, bigLogin.status);

const badHub = await publicRequest("/mcp", { host: "UPPER.relay.localhost" });
check("identifiant de hub invalide -> 404", badHub.status === 404, badHub.status);
const apex = await publicRequest("/mcp", { host: "relay.localhost" });
check("domaine du relais hors /connect -> 404", apex.status === 404, apex.status);

hub.ws.close();
console.log("TOUT EST BON");
process.exit(0);
