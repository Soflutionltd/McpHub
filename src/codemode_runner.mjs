// McpHub code mode : exécute le code de l'agent dans un processus Node isolé
// (node --permission : ni fichiers, ni réseau, ni sous-processus ; sandbox-exec en plus).
// Le seul accès vers l'extérieur est `mcp.call` / `mcp.search`, relayé par McpHub sur
// stdin/stdout, donc soumis à toutes ses gardes (permissions, quarantaine, profils, audit).
import readline from "node:readline";

const rl = readline.createInterface({ input: process.stdin, terminal: false });
const pending = new Map();
let nextId = 1;
let started = false;
const logs = [];
let logSize = 0;

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function request(kind, payload) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    send({ t: kind, id, ...payload });
  });
}

function fmt(v) {
  if (typeof v === "string") return v;
  try { return JSON.stringify(v); } catch { return String(v); }
}

const capture = (...args) => {
  const line = args.map(fmt).join(" ");
  if (logSize < 20000) { logs.push(line); logSize += line.length; }
};

const mcp = Object.freeze({
  call: (server, tool, args = {}) => {
    if (typeof server !== "string" || typeof tool !== "string") {
      return Promise.reject(new Error("mcp.call(server, tool, args) : server et tool doivent etre des chaines"));
    }
    return request("call", { server, tool, args });
  },
  search: (query, server) => request("search", { query: String(query ?? ""), server }),
});

async function run(code) {
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  const fn = new AsyncFunction("mcp", "console", code);
  const sandboxConsole = Object.freeze({ log: capture, info: capture, warn: capture, error: capture });
  return await fn(mcp, sandboxConsole);
}

rl.on("line", async (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (!started && msg.t === "start") {
    started = true;
    try {
      const result = await run(String(msg.code ?? ""));
      send({ t: "done", result: result === undefined ? null : result, logs });
    } catch (e) {
      send({ t: "error", message: String(e && e.stack ? e.stack : e), logs });
    }
    process.exit(0);
  }
  if (msg.id && pending.has(msg.id)) {
    const p = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.ok) p.resolve(msg.result);
    else p.reject(new Error(msg.error || "erreur"));
  }
});
