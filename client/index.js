#!/usr/bin/env node

import { open, readFile, rename, stat, unlink, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, normalize, parse, sep } from 'node:path';

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { SSEClientTransport } from '@modelcontextprotocol/sdk/client/sse.js';
import {
  getDefaultEnvironment,
  StdioClientTransport
} from '@modelcontextprotocol/sdk/client/stdio.js';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { ErrorCode, McpError } from '@modelcontextprotocol/sdk/types.js';
import { encode as encodeToon } from '@toon-format/toon';
import { z } from 'zod';

// Instance nommée (un McpHub par client) : mêmes règles que le moteur Rust.
const instanceName = process.env.MCPHUB_INSTANCE || undefined;
if (instanceName && !/^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?$/.test(instanceName)) {
  throw new Error(
    `MCPHUB_INSTANCE="${instanceName}" is invalid: use 1 to 32 lowercase letters, digits or dashes.`
  );
}
if (instanceName && !process.env.MCPHUB_PORT) {
  throw new Error('MCPHUB_PORT is required with MCPHUB_INSTANCE (each instance has its own port).');
}
const instanceEnv = Object.fromEntries(
  ['MCPHUB_INSTANCE', 'MCPHUB_PORT', 'MCPHUB_HOME', 'MCPHUB_VAULT_SERVICE']
    .filter((key) => process.env[key])
    .map((key) => [key, process.env[key]])
);

const hubDirectory =
  process.env.MCPHUB_HOME ||
  join(homedir(), instanceName ? `.McpHub-${instanceName}` : '.McpHub');
const projectsPath = process.env.MCPHUB_PROJECTS ?? join(hubDirectory, 'projects.json');
const usagePath = join(hubDirectory, 'client-usage-stats.json');
const usageLockPath = `${usagePath}.lock`;
const schemaCachePath = join(hubDirectory, 'schema-cache.json');
const authTokenPath = join(hubDirectory, 'auth-token');
const hubBinaryPath = process.env.MCPHUB_BINARY ?? join(homedir(), '.local', 'bin', 'McpHub');
const daemonBaseUrl =
  process.env.MCPHUB_DAEMON_URL ?? `http://127.0.0.1:${process.env.MCPHUB_PORT || 24680}`;
const daemonSseUrl = new URL('/sse', daemonBaseUrl);
const daemonApiUrl = new URL('/api/servers', daemonBaseUrl);

// Cursor limite un appel d'outil à quelques minutes ; on laisse une marge large
// mais bornée pour ne jamais laisser une requête pendre indéfiniment.
const childCallTimeoutMs = Number(process.env.MCPHUB_CALL_TIMEOUT_MS ?? 600_000);
const daemonProbeTimeoutMs = 1_500;
// Délai au-delà duquel un appel reçoit une réponse d'attente au lieu de laisser le
// client MCP le couper, et nombre de résultats terminés gardés en attente de lecture.
const answerBeforeMs = Number(process.env.MCPHUB_ANSWER_BEFORE_MS ?? 50_000);
if (!Number.isFinite(answerBeforeMs) || answerBeforeMs < 100) {
  throw new Error(
    `Invalid MCPHUB_ANSWER_BEFORE_MS "${process.env.MCPHUB_ANSWER_BEFORE_MS}": expected a number of milliseconds, 100 or more.`
  );
}
const pendingCallLimit = 20;
const daemonRetryIntervalMs = Number(process.env.MCPHUB_DAEMON_RETRY_MS ?? 60_000);
const usageFlushDelayMs = 2_000;
// Verrou du fichier de statistiques : attente maximale, et âge au-delà duquel il est abandonné.
const usageLockWaitMs = 2_000;
const usageLockStaleMs = 5_000;
const unknownSuggestionCount = 5;
// Au-delà, le texte d'un résultat est coupé et le reste se lit par poignée.
const defaultResultMaxChars = Number(process.env.MCPHUB_RESULT_MAX_CHARS ?? 60_000);
if (!Number.isInteger(defaultResultMaxChars) || defaultResultMaxChars < 1_000) {
  throw new Error(
    `MCPHUB_RESULT_MAX_CHARS="${process.env.MCPHUB_RESULT_MAX_CHARS}" is invalid: use an integer of at least 1000.`
  );
}
const storedResultLimit = 16;
// Au-delà, un résultat en lignes rappelle à l'agent fields et max_rows.
const shapingHintMinChars = 8_000;
const hubLocalServer = 'mcphub';

const cursorCatalogDescriptionLimit = 200;
// Nom du serveur dans la config MCP du client : Claude Code préfixe chaque
// outil par mcp__<nom>__.
const hubServerName = 'McpHub';

// Trace des injections du hook SessionStart de MemoryPilot : workspace →
// horodatage de la dernière injection réussie. À côté de MEMORYPILOT_DB quand
// il est défini (tests), sinon dans state/ (MemoryPilot --migrate-layout),
// sinon à la racine pour une installation pas encore rangée.
const memoryPilotRoot = join(homedir(), '.MemoryPilot');
const hookInjectionsPath = process.env.MEMORYPILOT_DB
  ? join(dirname(process.env.MEMORYPILOT_DB), 'session-hook-injections.json')
  : existsSync(join(memoryPilotRoot, 'state'))
    ? join(memoryPilotRoot, 'state', 'session-hook-injections.json')
    : join(memoryPilotRoot, 'session-hook-injections.json');
// Au-delà, la session a pu être compactée ou reprise ailleurs : recall complet.
const hookInjectionMaxAgeMs = 10 * 60_000;

// projects.json est relu dès qu'il change (date et taille) : une règle ajoutée ou
// corrigée s'applique à l'appel suivant, sans relancer le client.
let projectsConfig = { projects: {} };
let serverOwners = new Map();
let projectsFingerprint;
await refreshProjects();

/** @type {{ client: Client, transport: 'daemon' | 'stdio' } | undefined} */
let hubConnection;
/** @type {Promise<{ client: Client, transport: 'daemon' | 'stdio' }> | undefined} */
let hubConnecting;
let hubCallsInFlight = 0;
let lastDaemonProbeAt = 0;
/** @type {Map<string, Promise<{ memory?: string, memoryError?: string }>>} */
const pendingBootstraps = new Map();

// Plusieurs fenêtres partagent client-usage-stats.json : chacune ne garde en mémoire que ce
// qu'elle a compté depuis sa dernière écriture, et l'ajoute aux totaux du fichier.
const usageCounters = [
  'calls',
  'searches',
  'batches',
  'bootstrapCalls',
  'suggestionsServed',
  'runs',
  'runCalls',
  'childResultBytes'
];

function emptyUsage() {
  return {
    ...Object.fromEntries(usageCounters.map((counter) => [counter, 0])),
    // Sans prototype : un nom de serveur vient de l'agent et sert de clé.
    byServer: Object.create(null)
  };
}

const countOf = (value) => (Number.isFinite(value) && value > 0 ? value : 0);

function addUsage(target, added) {
  for (const counter of usageCounters) target[counter] += countOf(added?.[counter]);
  for (const [server, entry] of Object.entries(added?.byServer ?? {})) {
    const total = (target.byServer[server] ??= { calls: 0, resultBytes: 0 });
    total.calls += countOf(entry?.calls);
    total.resultBytes += countOf(entry?.resultBytes);
  }
  return target;
}

async function readUsageTotals() {
  const stored = await readJson(usagePath, {});
  return {
    ...addUsage(emptyUsage(), stored),
    startedAt: typeof stored?.startedAt === 'string' ? stored.startedAt : new Date().toISOString()
  };
}

let usage = emptyUsage();

async function readJson(path, fallback) {
  try {
    return JSON.parse(await readFile(path, 'utf8'));
  } catch {
    return fallback;
  }
}

// Sans fichier, aucune politique de projet. Un fichier illisible est une
// erreur : l'ignorer lèverait en silence les restrictions qu'il impose.
async function readProjects(path) {
  try {
    return JSON.parse(await readFile(path, 'utf8'));
  } catch (error) {
    if (error.code === 'ENOENT') return { projects: {} };
    throw new Error(`Invalid McpHub projects file ${path}: ${error.message}`);
  }
}

async function readProjectsFingerprint() {
  try {
    const { mtimeMs, size } = await stat(projectsPath);
    return `${mtimeMs}:${size}`;
  } catch (error) {
    if (error.code === 'ENOENT') return 'absent';
    throw new Error(`Invalid McpHub projects file ${projectsPath}: ${error.message}`);
  }
}

// Un fichier devenu invalide fait échouer chaque appel jusqu'à sa correction :
// garder l'ancienne politique masquerait l'erreur.
async function refreshProjects() {
  const fingerprint = await readProjectsFingerprint();
  if (fingerprint === projectsFingerprint) return;
  const config = await readProjects(projectsPath);
  const owners = buildServerOwners(config.projects);
  projectsConfig = config;
  serverOwners = owners;
  projectsFingerprint = fingerprint;
}

// Un serveur rattaché à un client (ownedServers) n'appartient qu'à un projet :
// deux propriétaires rendraient la règle ambiguë.
function buildServerOwners(projects) {
  const owners = new Map();
  for (const [name, config] of Object.entries(projects)) {
    const owned = config.ownedServers ?? [];
    if (!Array.isArray(owned)) {
      throw new Error(`Invalid McpHub projects file ${projectsPath}: "${name}".ownedServers must be an array.`);
    }
    for (const server of owned) {
      const previous = owners.get(server);
      if (previous) {
        throw new Error(
          `Invalid McpHub projects file ${projectsPath}: server "${server}" is owned by both "${previous}" and "${name}".`
        );
      }
      owners.set(server, name);
    }
  }
  return owners;
}

// ---------------------------------------------------------------------------
// Statistiques d'usage : une écriture disque toutes les 2 s au plus, plus un
// flush à la sortie, au lieu de deux écritures synchrones par appel.
// ---------------------------------------------------------------------------

let usageDirty = false;
let usageFlushTimer;
let usageFlushing = Promise.resolve();

function markUsageDirty() {
  usageDirty = true;
  if (usageFlushTimer) return;
  usageFlushTimer = setTimeout(() => {
    usageFlushTimer = undefined;
    void flushUsage();
  }, usageFlushDelayMs);
  usageFlushTimer.unref?.();
}

// Ce que l'agent reçoit réellement d'un serveur, après réduction du résultat :
// c'est ce volume qui entre dans son contexte.
function recordServerUsage(server, delivered) {
  const entry = (usage.byServer[server] ??= { calls: 0, resultBytes: 0 });
  entry.calls += 1;
  entry.resultBytes += Buffer.byteLength(JSON.stringify(delivered) ?? '');
  markUsageDirty();
}

async function withUsageLock(task) {
  const deadline = Date.now() + usageLockWaitMs;
  for (;;) {
    try {
      await (await open(usageLockPath, 'wx', 0o600)).close();
      break;
    } catch (error) {
      if (error?.code !== 'EEXIST') throw error;
      const lock = await stat(usageLockPath).catch(() => undefined);
      if (!lock) continue;
      if (Date.now() - lock.mtimeMs > usageLockStaleMs) {
        // Une fenêtre arrêtée pendant son écriture a laissé le verrou.
        await unlink(usageLockPath).catch(() => {});
        continue;
      }
      if (Date.now() > deadline) throw new Error('client-usage-stats.json is locked by another window');
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  }
  try {
    return await task();
  } finally {
    await unlink(usageLockPath).catch(() => {});
  }
}

async function mergeUsageIntoFile() {
  // Échangé avant toute attente : ce qui est compté pendant l'écriture part à la suivante.
  const counted = usage;
  usage = emptyUsage();
  try {
    await withUsageLock(async () => {
      const totals = addUsage(await readUsageTotals(), counted);
      const temporaryPath = `${usagePath}.${process.pid}.tmp`;
      try {
        await writeFile(temporaryPath, `${JSON.stringify(totals, null, 2)}\n`, { mode: 0o600 });
        await rename(temporaryPath, usagePath);
      } catch (error) {
        await unlink(temporaryPath).catch(() => {});
        throw error;
      }
    });
  } catch {
    // Les statistiques ne doivent jamais faire échouer un appel d'outil : ce qui
    // n'a pas pu être écrit reste compté pour la prochaine écriture.
    addUsage(usage, counted);
    usageDirty = true;
  }
}

function flushUsage() {
  if (!usageDirty) return usageFlushing;
  usageDirty = false;
  usageFlushing = usageFlushing.then(mergeUsageIntoFile);
  return usageFlushing;
}

// Totaux de toutes les fenêtres, plus ce que celle-ci n'a pas encore pu écrire.
async function readSharedUsage() {
  await flushUsage();
  return addUsage(await readUsageTotals(), usage);
}

for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.once(signal, () => {
    void flushUsage().finally(() => process.exit(0));
  });
}
process.stdin.once('close', () => {
  void flushUsage().finally(() => process.exit(0));
});

// ---------------------------------------------------------------------------
// Helpers MCP
// ---------------------------------------------------------------------------

function textResult(value, isError = false) {
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  return {
    content: [{ type: 'text', text }],
    ...(isError ? { isError: true } : {})
  };
}

function extractText(result) {
  return (result.content ?? [])
    .filter((item) => item.type === 'text')
    .map((item) => item.text)
    .join('\n');
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

// ---------------------------------------------------------------------------
// Résultats allégés : le texte des outils enfants est le premier poste de
// tokens. Sur un résultat JSON réussi, on retire les champs null ou vides, on
// applique fields et max_rows, puis on coupe au-delà de max_chars en gardant
// le texte complet lisible par morceaux (server "mcphub", tool "read_result").
// raw: true renvoie le résultat de l'enfant tel quel.
// ---------------------------------------------------------------------------

const shapingFields = {
  fields: z
    .array(z.string())
    .min(1)
    .optional()
    .describe('Keep only these keys in each row of a JSON result.'),
  max_rows: z
    .number()
    .int()
    .min(1)
    .optional()
    .describe('Keep only the first rows of each JSON array of the result.'),
  max_chars: z
    .number()
    .int()
    .min(1_000)
    .optional()
    .describe('Cut the result text beyond this size; the rest stays readable by handle.'),
  format: z
    .enum(['json', 'toon', 'csv'])
    .optional()
    .describe('toon writes a JSON result as TOON: each list of rows as one header line plus one line per row. csv is the most compact for a plain list of rows: one header line, one line per row.'),
  raw: z
    .boolean()
    .optional()
    .describe('Return the child result untouched (no pruning, no cut).')
};

function pickShaping({ fields, max_rows, max_chars, format, raw }) {
  return { fields, max_rows, max_chars, format, raw };
}

/** @type {Map<string, { text: string, server: string }>} poignée → texte complet, les plus anciens sortent en premier */
const storedResults = new Map();
let storedResultSequence = 0;

function storeFullResult(text, server) {
  storedResultSequence += 1;
  const handle = `r${storedResultSequence}`;
  storedResults.set(handle, { text, server });
  while (storedResults.size > storedResultLimit) {
    storedResults.delete(storedResults.keys().next().value);
  }
  return handle;
}

function isHighSurrogate(code) {
  return code >= 0xd800 && code <= 0xdbff;
}

function isLowSurrogate(code) {
  return code >= 0xdc00 && code <= 0xdfff;
}

// Coupe sans séparer une paire de substitution UTF-16 : la paire de fin est
// laissée au morceau suivant, sauf si le morceau en serait vide.
function sliceText(text, start, length) {
  let end = Math.min(text.length, start + length);
  if (end < text.length && isHighSurrogate(text.charCodeAt(end - 1))) {
    end += end - 1 === start ? 1 : -1;
  }
  return text.slice(start, end);
}

function pruneEmpty(value) {
  if (Array.isArray(value)) return value.map(pruneEmpty);
  if (value === null || typeof value !== 'object') return value;
  return Object.fromEntries(
    Object.entries(value)
      .filter(([, child]) => child !== null && child !== '')
      .map(([key, child]) => [key, pruneEmpty(child)])
  );
}

function isRow(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

// Les « lignes » d'un résultat : le tableau racine, ou chaque tableau d'objets
// porté par une clé de l'objet racine ({ projects: [...] }, { data: [...] }).
function shapeRows(value, { fields, max_rows }, notes) {
  const shapeArray = (rows, label) => {
    let shaped = rows;
    if (max_rows && shaped.length > max_rows) {
      notes.push(`${label}: kept ${max_rows} of ${shaped.length} rows (max_rows).`);
      shaped = shaped.slice(0, max_rows);
    }
    if (fields) {
      shaped = shaped.map((row) =>
        isRow(row)
          ? Object.fromEntries(fields.filter((key) => key in row).map((key) => [key, row[key]]))
          : row
      );
    }
    return shaped;
  };

  if (Array.isArray(value)) return shapeArray(value, 'result');
  const hasRows = (child) => Array.isArray(child) && child.some(isRow);
  if (!isRow(value) || !Object.values(value).some(hasRows)) {
    notes.push('fields and max_rows ignored: no array of rows at the root of this result.');
    return value;
  }
  return Object.fromEntries(
    Object.entries(value).map(([key, child]) => [key, hasRows(child) ? shapeArray(child, key) : child])
  );
}

// Un nombre de 16 chiffres ou plus (identifiant X, TikTok, Stripe) perd sa
// précision dans un aller-retour JSON.parse / JSON.stringify, et un exposant
// hors plage (1e400) y devient null : le résultat qui en contient un n'est pas
// réécrit.
function hasUnsafeNumber(text) {
  let inString = false;
  let digits = 0;
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    if (inString) {
      if (code === 0x5c) index += 1;
      else if (code === 0x22) inString = false;
      continue;
    }
    if (code === 0x22) {
      inString = true;
      digits = 0;
    } else if (code >= 0x30 && code <= 0x39) {
      digits += 1;
      if (digits >= 16) return true;
    } else if ((code === 0x65 || code === 0x45) && digits > 0) {
      return true;
    } else if (code !== 0x2e) {
      digits = 0;
    }
  }
  return false;
}

// Nombre de lignes du plus grand tableau de lignes du résultat (voir shapeRows).
function countRows(value) {
  const rowsOf = (child) => (Array.isArray(child) && child.some(isRow) ? child.length : 0);
  if (Array.isArray(value)) return rowsOf(value);
  if (!isRow(value)) return 0;
  return Math.max(0, ...Object.values(value).map(rowsOf));
}

function parseJsonContainer(text) {
  const first = text.trimStart()[0];
  if (first !== '{' && first !== '[') return undefined;
  if (hasUnsafeNumber(text)) return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function shapeText(text, shaping, notes, server) {
  let shaped = text;
  let json = parseJsonContainer(text);
  if (json !== undefined) {
    json = pruneEmpty(json);
    if (shaping.fields || shaping.max_rows) json = shapeRows(json, shaping, notes);
    shaped = JSON.stringify(json);
    const rows = shaping.fields || shaping.max_rows ? 0 : countRows(json);
    if (rows > 1 && shaped.length >= shapingHintMinChars) {
      notes.push(
        `Large result: ${shaped.length} chars for ${rows} rows. Next time pass fields (keys to keep) and max_rows to this call.`
      );
    }
    if (shaping.format === 'toon') {
      shaped = encodeToon(json);
      json = undefined;
    } else if (shaping.format === 'csv') {
      const csv = encodeCsv(json);
      if (csv === undefined) {
        notes.push('csv needs a list of rows (or an object holding one): sent as JSON.');
      } else {
        shaped = csv;
        json = undefined;
      }
    }
  } else if (/^\s*[{[]/.test(text)) {
    // Du JSON que McpHub ne réécrit pas (nombre de 16 chiffres ou plus, texte coupé ou invalide) :
    // il ressort tel quel, donc sans nettoyage non plus.
    if (shaping.format && shaping.format !== 'json') {
      notes.push(`format ${shaping.format} not applied: this JSON holds a number too large to rewrite safely, or is not complete.`);
    }
  } else {
    const quiet = removeTerminalNoise(text);
    if (quiet.length < text.length) {
      notes.push(`Terminal noise removed (${text.length - quiet.length} chars: colour codes, progress redraws, long lines repeated in a row). raw: true gives the original.`);
      shaped = quiet;
    }
  }
  const limit = shaping.max_chars ?? defaultResultMaxChars;
  if (shaped.length <= limit) return { text: shaped, json };

  const handle = storeFullResult(shaped, server);
  const head = sliceText(shaped, 0, limit);
  notes.push(
    `Result cut at ${head.length} of ${shaped.length} chars. To find something in the rest without reading it all: mcphub_call server "mcphub", tool "search_result", arguments {"handle":"${handle}","query":"words to find"}. To read on: tool "read_result", arguments {"handle":"${handle}","offset":${head.length}}.`
  );
  return { text: head, json: undefined };
}

// ---------------------------------------------------------------------------
// CSV : pour une liste de lignes, une ligne d'en-tête puis une ligne par
// enregistrement. Accepte un tableau de lignes, ou un objet qui en contient un
// seul (ses autres champs passent en tête, un par ligne "# clé: valeur").
// ---------------------------------------------------------------------------

function csvCell(value) {
  if (value === undefined || value === null) return '';
  const text = typeof value === 'object' ? JSON.stringify(value) : String(value);
  return /[",\n\r]/.test(text) || text !== text.trim() ? `"${text.replace(/"/g, '""')}"` : text;
}

function encodeCsv(json) {
  let rows = json;
  const head = [];
  if (!Array.isArray(json)) {
    if (!isRow(json)) return undefined;
    const lists = Object.entries(json).filter(([, value]) => Array.isArray(value) && value.some(isRow));
    if (lists.length !== 1) return undefined;
    rows = lists[0][1];
    for (const [key, value] of Object.entries(json)) {
      // Une valeur sur plusieurs lignes passerait pour des lignes de données : elle est écrite en JSON.
      if (key !== lists[0][0]) head.push(`# ${key}: ${typeof value === 'object' || /[\n\r]/.test(String(value)) ? JSON.stringify(value) : value}`);
    }
    head.push(`# ${lists[0][0]}:`);
  }
  if (!rows.length || !rows.every(isRow)) return undefined;
  const columns = [];
  const known = new Set();
  for (const row of rows) {
    for (const key of Object.keys(row)) {
      if (!known.has(key)) {
        known.add(key);
        columns.push(key);
      }
    }
  }
  if (!columns.length) return undefined;
  return [...head, columns.map(csvCell).join(','), ...rows.map((row) => columns.map((key) => csvCell(row[key])).join(','))].join('\n');
}

// ---------------------------------------------------------------------------
// Bruit de terminal : ce qu'un écran efface et qu'un journal garde. Codes de
// couleur, barres de progression redessinées sur place (retour chariot), et
// la même longue ligne répétée cinq fois ou plus. Une ligne courte répétée
// ("0", "}", "---") est une donnée, pas du bruit : elle reste. Rien d'autre
// n'est touché : un fichier source ou un texte ordinaire ressort identique.
// ---------------------------------------------------------------------------

const ansiSequence = /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)/g;
const repeatedLineMin = 5;
const repeatedLineMinChars = 20;

function removeTerminalNoise(text) {
  if (!/[\u001b\r]/.test(text) && !/\n/.test(text)) return text;
  const out = [];
  let run = 0;
  const flush = () => {
    if (run >= repeatedLineMin) {
      out.length -= run - 1;
      out.push(`[McpHub: the line above came ${run} times in a row]`);
    }
    run = 0;
  };
  for (const rawLine of text.replace(ansiSequence, '').split('\n')) {
    // Une barre de progression réécrit sa ligne : seul le dernier état compte.
    // Une fin de ligne Windows (un seul retour chariot final) n'en est pas une : elle reste.
    const body = rawLine.replace(/\r+$/, '');
    const line = body.includes('\r') ? body.split('\r').pop() : rawLine;
    if (run > 0 && line === out[out.length - 1] && line.trim().length >= repeatedLineMinChars) run += 1;
    else {
      flush();
      run = 1;
    }
    out.push(line);
  }
  flush();
  return out.join('\n');
}

// Renvoie le résultat allégé et, pour mcphub_batch, la valeur à insérer telle
// quelle (objet JSON plutôt que chaîne échappée) avec les notes à part.
function shapeChildResult(result, shaping, server) {
  if (shaping.raw || result.isError || callStatus(result) !== 'ok') {
    return { result, value: extractText(result), notes: [] };
  }
  const notes = [];
  const shapedTexts = [];
  const content = (result.content ?? []).map((item) => {
    if (item.type !== 'text') return item;
    const shapedText = shapeText(item.text, shaping, notes, server);
    shapedTexts.push(shapedText);
    return { ...item, text: shapedText.text };
  });
  const value =
    shapedTexts.length === 1 && shapedTexts[0].json !== undefined
      ? shapedTexts[0].json
      : shapedTexts.map((shapedText) => shapedText.text).join('\n');
  if (notes.length) content.push({ type: 'text', text: `[McpHub] ${notes.join(' ')}` });
  return { result: { ...result, content }, value, notes };
}

function readStoredResult({ handle, offset = 0, length }, workingDirectory) {
  const stored = typeof handle === 'string' ? storedResults.get(handle) : undefined;
  // Un serveur rattaché à un projet ne se relit pas depuis un autre dossier.
  const full = stored && ownerAllows(stored.server, workingDirectory) ? stored.text : undefined;
  if (full === undefined) {
    return textResult(
      `Unknown result handle "${handle}": handles live in this McpHub client process and only the last ${storedResultLimit} cut results are kept. Call the tool again.`,
      true
    );
  }
  if (!Number.isInteger(offset) || offset < 0 || offset > full.length) {
    return textResult(`offset must be an integer between 0 and ${full.length}.`, true);
  }
  if (length !== undefined && (!Number.isInteger(length) || length < 1)) {
    return textResult('length must be a positive integer.', true);
  }
  if (offset > 0 && isLowSurrogate(full.charCodeAt(offset)) && isHighSurrogate(full.charCodeAt(offset - 1))) {
    return textResult(`offset ${offset} is inside a character: use ${offset - 1} or ${offset + 1}.`, true);
  }
  const chunk = sliceText(full, offset, length ?? defaultResultMaxChars);
  const next = offset + chunk.length;
  return {
    content: [
      { type: 'text', text: chunk },
      {
        type: 'text',
        text:
          next < full.length
            ? `[McpHub] chars ${offset} to ${next} of ${full.length}. Next offset: ${next}.`
            : `[McpHub] chars ${offset} to ${next} of ${full.length}. End of result.`
      }
    ]
  };
}

// Cherche dans un résultat gardé sans le relire en entier : les passages où les mots de la
// requête apparaissent, les plus complets d'abord, chacun avec sa position pour read_result.
function searchStoredResult({ handle, query, context = 200, max_matches = 8 }, workingDirectory) {
  const stored = typeof handle === 'string' ? storedResults.get(handle) : undefined;
  const full = stored && ownerAllows(stored.server, workingDirectory) ? stored.text : undefined;
  if (full === undefined) {
    return textResult(
      `Unknown result handle "${handle}": handles live in this McpHub client process and only the last ${storedResultLimit} cut results are kept. Call the tool again.`,
      true
    );
  }
  const terms = typeof query === 'string' ? [...new Set(query.toLowerCase().split(/\s+/).filter(Boolean))].slice(0, 12) : [];
  if (!terms.length) return textResult('query must be one or more words to find.', true);
  if (!Number.isInteger(context) || context < 20 || context > 2000) return textResult('context must be an integer between 20 and 2000.', true);
  if (!Number.isInteger(max_matches) || max_matches < 1 || max_matches > 50) return textResult('max_matches must be an integer between 1 and 50.', true);

  // Recherche sans tenir compte de la casse directement dans le texte : passer le texte en
  // minuscules changerait sa longueur pour certaines lettres et fausserait toutes les positions.
  // Chaque mot a son propre plafond : un mot très fréquent ne cache pas les autres.
  const hits = [];
  const hitLimitPerTerm = 5_000;
  let capped = false;
  const found = terms.map(() => false);
  terms.forEach((term, index) => {
    const pattern = new RegExp(term.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
    let count = 0;
    for (let match = pattern.exec(full); match; match = pattern.exec(full)) {
      found[index] = true;
      if (++count > hitLimitPerTerm) {
        capped = true;
        break;
      }
      hits.push({ at: match.index, end: match.index + match[0].length, index });
    }
  });
  if (!hits.length) {
    return textResult(`[McpHub] No match for ${terms.map((term) => JSON.stringify(term)).join(', ')} in the ${full.length} chars of ${handle}.`);
  }
  hits.sort((a, b) => a.at - b.at);
  // Les occurrences proches forment un passage ; un passage qui réunit plus de mots passe devant.
  const passages = [];
  for (const hit of hits) {
    const last = passages[passages.length - 1];
    if (last && hit.at - last.end <= context && hit.end - last.start <= context * 6) {
      last.end = Math.max(last.end, hit.end);
      last.terms.add(hit.index);
      last.count += 1;
    } else passages.push({ start: hit.at, end: hit.end, terms: new Set([hit.index]), count: 1 });
  }
  // Un mot rare pèse plus qu'un mot présent partout : le passage qui tient « error » passe
  // devant les milliers de passages qui ne tiennent que « the ».
  const frequency = terms.map((_, index) => hits.reduce((count, hit) => count + (hit.index === index ? 1 : 0), 0));
  const weight = (passage) => [...passage.terms].reduce((sum, index) => sum + 1 / frequency[index], 0);
  const ranked = [...passages]
    .map((passage) => ({ ...passage, weight: weight(passage) }))
    .sort((a, b) => b.terms.size - a.terms.size || b.weight - a.weight || a.start - b.start)
    .slice(0, max_matches);
  // La réponse reste petite : les meilleurs passages d'abord, jusqu'au budget ; les autres sont
  // seulement comptés. Ils sont ensuite rendus dans l'ordre du texte.
  const budget = 20_000;
  const chosen = [];
  let used = 0;
  for (const passage of ranked) {
    let from = Math.max(0, passage.start - context);
    if (from > 0 && isLowSurrogate(full.charCodeAt(from)) && isHighSurrogate(full.charCodeAt(from - 1))) from -= 1;
    const excerpt = sliceText(full, from, passage.end - from + context);
    if (chosen.length && used + excerpt.length > budget) continue;
    used += excerpt.length;
    chosen.push({ from, excerpt });
  }
  const content = chosen
    .sort((a, b) => a.from - b.from)
    .map(({ from, excerpt }) => ({ type: 'text', text: `[chars ${from} to ${from + excerpt.length}]\n${excerpt}` }));
  const missing = terms.filter((_, index) => !found[index]);
  content.push({
    type: 'text',
    text: `[McpHub] ${content.length} of ${passages.length} passages shown, ${hits.length}${capped ? '+' : ''} matches in ${full.length} chars.` +
      (missing.length ? ` Never found: ${missing.map((term) => JSON.stringify(term)).join(', ')}.` : '') +
      ' Read around one with read_result and its offset.'
  });
  return { content };
}

// ---------------------------------------------------------------------------
// Connexion au hub : daemon partagé (SSE) en priorité, McpHub stdio en secours.
//
// Le daemon `McpHub serve` (launchd) garde les serveurs enfants chauds et les
// partage entre toutes les fenêtres Cursor. Le mode stdio en spawn un jeu
// complet par fenêtre : on ne l'utilise que si le daemon est injoignable.
// ---------------------------------------------------------------------------

async function daemonReachable() {
  try {
    const token = await readAuthToken();
    const response = await fetch(daemonApiUrl, {
      headers: { Authorization: `Bearer ${token}` },
      signal: AbortSignal.timeout(daemonProbeTimeoutMs)
    });
    return response.ok;
  } catch {
    return false;
  }
}

async function readAuthToken() {
  return (await readFile(authTokenPath, 'utf8')).trim();
}

function createHubClient() {
  return new Client({ name: 'mcphub-client', version: '1.4.0' });
}

async function connectDaemon() {
  const token = await readAuthToken();
  const client = createHubClient();
  const transport = new SSEClientTransport(daemonSseUrl, {
    requestInit: { headers: { Authorization: `Bearer ${token}` } }
  });
  await client.connect(transport);
  return { client, transport: 'daemon' };
}

async function connectStdio() {
  const client = createHubClient();
  const transport = new StdioClientTransport({
    command: hubBinaryPath,
    args: ['stdio'],
    env: { ...getDefaultEnvironment(), ...instanceEnv },
    stderr: 'ignore'
  });
  await client.connect(transport);
  return { client, transport: 'stdio' };
}

async function openHubConnection() {
  lastDaemonProbeAt = Date.now();
  const connection = (await daemonReachable())
    ? await connectDaemon()
    : await connectStdio();
  connection.client.onclose = () => {
    if (hubConnection?.client === connection.client) {
      hubConnection = undefined;
      hubConnecting = undefined;
    }
  };
  connection.client.onerror = () => {
    // La reconnexion est gérée par le retry de callHubTool.
  };
  return connection;
}

function connectHub() {
  if (hubConnection) return Promise.resolve(hubConnection);
  if (!hubConnecting) {
    hubConnecting = openHubConnection()
      .then((connection) => {
        hubConnection = connection;
        return connection;
      })
      .finally(() => {
        hubConnecting = undefined;
      });
  }
  return hubConnecting;
}

async function resetHubConnection() {
  const stale = hubConnection;
  hubConnection = undefined;
  hubConnecting = undefined;
  try {
    await stale?.client.close();
  } catch {
    // Le transport est déjà indisponible.
  }
}

// Une erreur JSON-RPC renvoyée par le hub (outil inconnu, arguments invalides,
// échec métier) est une réponse valide : on ne doit ni reconnecter ni rejouer.
function isTransportFailure(error) {
  if (!(error instanceof McpError)) return true;
  return error.code === ErrorCode.ConnectionClosed || isLocalTimeout(error);
}

// Le démon répond aussi -32001 pour ses refus (« Bloque par McpHub », serveur hors
// profil) : ce sont des réponses, pas des coupures. Seul le délai levé ici par le SDK,
// reconnaissable aux données qu'il joint, laisse l'issue de l'appel inconnue.
function isLocalTimeout(error) {
  return (
    error.code === ErrorCode.RequestTimeout &&
    (error.data?.timeout !== undefined || error.data?.maxTotalTimeout !== undefined)
  );
}

// La requête n'a jamais atteint le hub : transport fermé avant l'envoi, connexion
// refusée, ou session SSE inconnue (le démon la refuse avant toute exécution).
// Une coupure ou un timeout après l'envoi ne garantit rien : l'outil a pu s'exécuter.
const connectionRefusedCodes = new Set(['ECONNREFUSED', 'ENOENT', 'ENOTFOUND', 'EAI_AGAIN']);

function requestNeverReachedHub(error) {
  if (error instanceof McpError) return false;
  const message = errorMessage(error);
  if (message === 'Not connected') return true;
  if (message.startsWith('Error POSTing to endpoint (HTTP 404)')) return true;
  return connectionRefusedCodes.has(error?.cause?.code ?? error?.code);
}

class UnknownOutcomeError extends Error {
  constructor(name, cause) {
    super(
      `[issue inconnue] ${name} : la requête a été envoyée au hub mais aucune réponse n'est arrivée (${errorMessage(cause)}). ` +
        "L'action a pu s'exécuter : vérifier son effet avant de relancer."
    );
    this.status = 'unknown_outcome';
  }
}

// `replaySafe` : lecture pure (recherche, recall MemoryPilot), rejouable même après
// une coupure. Les appels d'outils et le code mode ne sont jamais rejoués une fois
// envoyés.
// Le secours stdio lance son propre McpHub, avec sa propre copie des jetons OAuth :
// dès que le démon répond de nouveau, on le referme pour revenir au démon partagé.
async function leaveStdioFallback() {
  if (hubConnection?.transport !== 'stdio' || hubCallsInFlight > 0) return;
  if (Date.now() - lastDaemonProbeAt < daemonRetryIntervalMs) return;
  lastDaemonProbeAt = Date.now();
  if (!(await daemonReachable())) return;
  if (hubConnection?.transport !== 'stdio' || hubCallsInFlight > 0) return;
  await resetHubConnection();
}

async function callHubTool(name, args, options = {}) {
  if (options.retry !== false) await leaveStdioFallback();
  hubCallsInFlight += 1;
  try {
    return await callHubToolOnce(name, args, options);
  } finally {
    hubCallsInFlight -= 1;
  }
}

async function callHubToolOnce(name, args, { replaySafe = false, retry = true } = {}) {
  let connection;
  try {
    connection = await connectHub();
  } catch (error) {
    if (!retry) throw error;
    await resetHubConnection();
    return callHubToolOnce(name, args, { replaySafe, retry: false });
  }
  try {
    const result = await connection.client.callTool(
      { name, arguments: args },
      undefined,
      {
        timeout: childCallTimeoutMs,
        resetTimeoutOnProgress: true,
        maxTotalTimeout: childCallTimeoutMs
      }
    );
    usage.childResultBytes += Buffer.byteLength(JSON.stringify(result));
    markUsageDirty();
    return result;
  } catch (error) {
    if (!isTransportFailure(error)) throw error;
    await resetHubConnection();
    if (retry && (replaySafe || requestNeverReachedHub(error))) {
      return callHubToolOnce(name, args, { replaySafe, retry: false });
    }
    if (requestNeverReachedHub(error)) throw error;
    throw new UnknownOutcomeError(name, error);
  }
}

// ---------------------------------------------------------------------------
// Politique projet
// ---------------------------------------------------------------------------

// `/Clients/Acme2` n'est pas dans le dossier `/Clients/Acme`.
function insideWorkspace(candidate, workspace) {
  const root = normalize(workspace).replace(/[\\/]+$/, '');
  return candidate === root || candidate.startsWith(root + sep);
}

function resolveProject(projectName, workingDirectory) {
  if (projectName && projectsConfig.projects[projectName]) {
    return [projectName, projectsConfig.projects[projectName]];
  }

  const candidate = normalize(workingDirectory || process.cwd());
  return (
    Object.entries(projectsConfig.projects).find(([, config]) =>
      config.workspaces.some((workspace) => insideWorkspace(candidate, workspace))
    ) ?? [null, null]
  );
}

// allowedServers: ["*"] laisse passer tous les serveurs : le projet ne garde que
// ses identifiants épinglés, et un serveur ajouté plus tard n'est pas bloqué.
function allowsEveryServer(projectConfig) {
  return !projectConfig.tenant && (projectConfig.allowedServers ?? []).includes('*');
}

function projectAllows(projectConfig, server) {
  return allowsEveryServer(projectConfig) || allowedServersOf(projectConfig).has(server);
}

function allowedServersOf(projectConfig) {
  // Tenant : ses serveurs et les serveurs partagés, rien d'autre (le démon applique
  // la même règle et reste l'autorité).
  if (projectConfig.tenant) {
    return new Set([
      ...Object.keys(projectConfig.credentials ?? {}),
      ...(projectConfig.sharedServers ?? []),
      ...(projectConfig.ownedServers ?? [])
    ]);
  }
  return new Set([...(projectConfig.allowedServers ?? []), ...(projectConfig.ownedServers ?? [])]);
}

// Un serveur rattaché à un client ne répond que si working_dir est dans le
// dossier de ce client : ni le paramètre `project` (que le modèle peut se
// tromper à passer) ni le dossier courant du processus ne suffisent.
function ownerAllows(server, workingDirectory) {
  const owner = serverOwners.get(server);
  if (!owner) return true;
  return Boolean(workingDirectory) && resolveProject(undefined, workingDirectory)[0] === owner;
}

function assertOwnership(server, workingDirectory) {
  if (ownerAllows(server, workingDirectory)) return;
  throw new Error(
    `Server "${server}" belongs to project "${serverOwners.get(server)}": it only answers when working_dir is inside that project's workspace.`
  );
}

function serverVisibility(projectConfig, workingDirectory) {
  return (server) =>
    (!projectConfig || projectAllows(projectConfig, server)) && ownerAllows(server, workingDirectory);
}

function enforceProjectPolicy({ projectName, projectConfig, server, tool, args, workingDirectory }) {
  assertOwnership(server, workingDirectory);
  if (!projectConfig) return { ...args };
  if (!projectAllows(projectConfig, server)) {
    throw new Error(
      `Server "${server}" is not allowed for project "${projectName}".`
    );
  }

  const securedArgs = { ...args };
  const aliases = projectConfig.aliases;

  if (
    server === 'supabase' &&
    securedArgs.project_id &&
    securedArgs.project_id !== aliases.supabaseProjectId
  ) {
    throw new Error(
      aliases.supabaseProjectId
        ? `Supabase project mismatch for ${projectName}: expected ${aliases.supabaseProjectId}.`
        : `No Supabase project is declared for ${projectName}: add aliases.supabaseProjectId to this project in ${projectsPath} (read again on the next call).`
    );
  }

  if (
    (server === 'app-store-connect' || server === 'appstore-operator') &&
    securedArgs.appId &&
    securedArgs.appId !== aliases.appStoreAppId
  ) {
    throw new Error(
      `App Store app mismatch for ${projectName}: expected ${aliases.appStoreAppId}.`
    );
  }

  if (
    server === 'cf-orchestrator' &&
    tool === 'add_project' &&
    securedArgs.cf_project_name &&
    securedArgs.cf_project_name !== aliases.cloudflareProject
  ) {
    throw new Error(
      `Cloudflare project mismatch for ${projectName}: expected ${aliases.cloudflareProject}.`
    );
  }

  return securedArgs;
}

// Dans un script mcphub_run les appels partent du moteur, pas d'ici : les
// règles du projet lui sont transmises et c'est lui qui les applique à chaque
// mcp.call. Reprend exactement ce que enforceProjectPolicy vérifie.
function runPolicyFor(projectConfig, workingDirectory) {
  const denied = [...serverOwners.keys()].filter((server) => !ownerAllows(server, workingDirectory));
  if (!projectConfig) return denied.length ? { denied_servers: denied } : undefined;

  const aliases = projectConfig.aliases ?? {};
  const pinned = {
    ...(aliases.supabaseProjectId ? { supabase: { project_id: aliases.supabaseProjectId } } : {}),
    ...(aliases.appStoreAppId
      ? {
          'app-store-connect': { appId: aliases.appStoreAppId },
          'appstore-operator': { appId: aliases.appStoreAppId }
        }
      : {}),
    ...(aliases.cloudflareProject
      ? { 'cf-orchestrator': { cf_project_name: aliases.cloudflareProject } }
      : {})
  };
  return {
    ...(allowsEveryServer(projectConfig)
      ? {}
      : {
          allowed_servers: [...allowedServersOf(projectConfig)].filter((server) =>
            ownerAllows(server, workingDirectory)
          )
        }),
    ...(denied.length ? { denied_servers: denied } : {}),
    ...(Object.keys(pinned).length ? { pinned_args: pinned } : {})
  };
}

// Un moteur plus ancien ignorerait la policy sans le dire : on ne lui confie un
// script soumis à des règles que s'il déclare savoir les appliquer.
async function daemonEnforcesRunPolicy() {
  const connection = await connectHub();
  if (connection.enforcesRunPolicy === undefined) {
    const { tools } = await connection.client.listTools();
    const run = tools.find((tool) => tool.name === 'mcphub_run');
    connection.enforcesRunPolicy = Boolean(run?.inputSchema?.properties?.policy);
  }
  return connection.enforcesRunPolicy;
}

// Dossiers de l'appelant, transmis au démon avec chaque appel : c'est lui qui en
// déduit le tenant (comptes d'un client) et refuse tout ce qui sort de son périmètre.
function hubContext(workingDirectory) {
  return {
    _mcphub_context: {
      ...(workingDirectory ? { working_dir: normalize(workingDirectory) } : {}),
      cwd: process.cwd()
    }
  };
}

// Tenant du dossier de l'appel (même règle que le démon : working_dir, sinon le
// dossier courant du client).
function tenantOf(workingDirectory) {
  const [, config] = resolveProject(undefined, workingDirectory || process.cwd());
  return config?.tenant;
}

// Un démon antérieur aux tenants ignorerait le dossier et utiliserait les comptes
// McpHub : depuis le dossier d'un tenant, on refuse plutôt que de fuiter.
async function daemonEnforcesTenants() {
  const connection = await connectHub();
  if (connection.enforcesTenants === undefined) {
    const { tools } = await connection.client.listTools();
    const call = tools.find((tool) => tool.name === 'mcphub_call');
    connection.enforcesTenants = Boolean(call?.inputSchema?.properties?._mcphub_context);
  }
  return connection.enforcesTenants;
}

async function assertTenantSupport(workingDirectory) {
  const tenant = tenantOf(workingDirectory);
  if (tenant && !(await daemonEnforcesTenants())) {
    throw new Error(
      `This folder belongs to tenant "${tenant}" but the running McpHub daemon is too old to isolate tenant accounts. Update McpHub before calling any server from here.`
    );
  }
}

// Le recall MemoryPilot n'est déclenché que par mcphub_bootstrap : il ne doit
// jamais retarder un appel Supabase ou Cloudflare qui n'en a pas besoin.
// Le processus vit aussi longtemps que la fenêtre Cursor, donc sur plusieurs
// conversations : seuls les appels simultanés identiques partagent un recall.
function recallContext(projectName, workspace, hints) {
  const key = JSON.stringify([projectName, workspace, hints || '']);
  const pending = pendingBootstraps.get(key);
  if (pending) return pending;

  const recall = callHubTool('mcphub_call', {
    ...hubContext(workspace),
    server: 'MemoryPilot',
    tool: 'recall',
    arguments: {
      working_dir: workspace,
      ...(projectName ? { project: projectName } : {}),
      hints: hints || 'project context',
      mode: 'safe',
      compact: true
    },
    bypass_cache: true
  }, { replaySafe: true })
    .then((result) => ({ memory: extractText(result) }))
    .catch((error) => ({ memoryError: errorMessage(error) }))
    .finally(() => pendingBootstraps.delete(key));

  pendingBootstraps.set(key, recall);
  return recall;
}

function isClaudeCodeClient() {
  return /^claude-code/i.test(server.server.getClientVersion()?.name ?? '');
}

// Une trace couvre son dossier exact, ou un dossier parent qui délimite un
// projet : une session ouverte dans ~ ou à la racine n'a pas reçu la mémoire
// des projets qu'ils contiennent.
function traceCovers(candidate, root) {
  const trimmed = (path) => normalize(path).replace(/[\\/]+$/, '') || sep;
  const base = trimmed(root);
  if (trimmed(candidate) === base) return true;
  if (base === parse(base).root || base === trimmed(homedir())) return false;
  return insideWorkspace(trimmed(candidate), base);
}

// Le hook SessionStart de MemoryPilot note chaque injection réussie. Pour
// Claude Code, une trace récente pour ce workspace veut dire que la mémoire est
// déjà dans le contexte : la rappeler ici la ferait payer deux fois. Les autres
// clients font toujours le recall complet.
async function recentHookInjection(workspace) {
  if (!isClaudeCodeClient()) return undefined;
  const traces = await readJson(hookInjectionsPath, {});
  const candidate = normalize(workspace);
  for (const [root, at] of Object.entries(traces ?? {})) {
    if (!traceCovers(candidate, root)) continue;
    const ageMs = Date.now() - Date.parse(at);
    if (ageMs >= 0 && ageMs < hookInjectionMaxAgeMs) return { injectedAt: at };
  }
  return undefined;
}

// Faits et jalons déjà injectés par le hook : seuls les épisodes liés à la
// tâche sont nouveaux, et MemoryPilot les filtre contre ce qu'il a injecté.
function recallEpisodes(projectName, workspace, hints) {
  if (!hints?.trim()) return Promise.resolve({ episodes: [] });
  return callHubTool('mcphub_call', {
    ...hubContext(workspace),
    server: 'MemoryPilot',
    tool: 'recall',
    arguments: {
      working_dir: workspace,
      ...(projectName ? { project: projectName } : {}),
      hints,
      episodes_only: true
    },
    bypass_cache: true
  }, { replaySafe: true })
    .then((result) => ({ episodes: JSON.parse(extractText(result)).episodes ?? [] }))
    .catch((error) => ({ memoryError: errorMessage(error) }));
}

function parseSearchPayload(result) {
  try {
    return JSON.parse(extractText(result));
  } catch {
    return null;
  }
}

function filterSearchResult(result, projectName, projectConfig, workingDirectory) {
  if (result.isError || (!projectConfig && serverOwners.size === 0)) return result;
  const payload = parseSearchPayload(result);
  if (!payload) {
    // Sans pouvoir filtrer, on ne renvoie pas les outils des autres clients.
    return textResult('McpHub search returned an unreadable payload; results withheld by the project policy.', true);
  }

  const visible = serverVisibility(projectConfig, workingDirectory);
  payload.results = (payload.results ?? []).filter((item) => visible(item.server));
  payload.available_servers = (payload.available_servers ?? []).filter(visible);
  if (!projectConfig) return textResult(payload);
  payload.project = {
    name: projectName,
    aliases: projectConfig.aliases,
    policy: 'enforced'
  };
  return textResult(payload);
}

// ---------------------------------------------------------------------------
// Suggestions automatiques : quand un appel vise un outil ou un serveur
// inconnu, on renvoie les meilleurs candidats BM25 dans l'erreur elle-même.
// Le modèle se corrige au tour suivant sans mcphub_search préventif.
// ---------------------------------------------------------------------------

// Selon le transport, le hub renvoie l'inconnu soit comme résultat isError,
// soit comme erreur JSON-RPC -32602 préfixée « MCP error -32602: ».
const unknownToolPattern = /\bUnknown tool '([^']+)' on server '([^']+)'/i;
const unknownServerPattern = /\bUnknown server:? '?([^'\s]+)'?/i;

// État réel d'un appel : ok, error, not_executed, unknown_outcome,
// confirmation_required. `isError` seul ne le dit pas (une demande de
// confirmation n'est pas une erreur, mais rien n'a été exécuté).
function errorToResult(error) {
  const status = error?.status ?? error?.data?.status ?? 'error';
  return { ...textResult(errorMessage(error), true), _meta: { mcphub: { status } } };
}

function callStatus(result) {
  return (
    result._meta?.mcphub?.status ??
    result._mcphub?.status ??
    (result.isError ? 'error' : 'ok')
  );
}

async function callChildWithSuggestions({
  server,
  tool,
  args,
  bypass_cache,
  confirmation_token,
  visible,
  workingDirectory
}) {
  let result;
  try {
    await assertTenantSupport(workingDirectory);
    result = await callHubTool('mcphub_call', {
      ...hubContext(workingDirectory),
      server,
      tool,
      arguments: args,
      bypass_cache,
      ...(confirmation_token ? { confirmation_token } : {})
    });
  } catch (error) {
    if (!(error instanceof McpError)) throw error;
    result = errorToResult(error);
  }
  return withUnknownSuggestions(result, { server, tool, visible });
}

// Cache de schémas du hub, relu seulement quand le fichier change.
let schemaCacheSnapshot = { mtimeMs: -1, servers: {} };

async function readSchemaServers() {
  const { mtimeMs } = await stat(schemaCachePath);
  if (mtimeMs !== schemaCacheSnapshot.mtimeMs) {
    const cache = JSON.parse(await readFile(schemaCachePath, 'utf8'));
    schemaCacheSnapshot = { mtimeMs, servers: cache.servers ?? {} };
  }
  return schemaCacheSnapshot.servers;
}

function tokenize(value) {
  return value
    .replace(/([a-z0-9])([A-Z])/g, '$1 $2')
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter(Boolean);
}

function levenshtein(left, right) {
  if (left === right) return 0;
  let previous = Array.from({ length: right.length + 1 }, (_, index) => index);
  for (let i = 1; i <= left.length; i += 1) {
    const current = [i];
    for (let j = 1; j <= right.length; j += 1) {
      current[j] = Math.min(
        previous[j] + 1,
        current[j - 1] + 1,
        previous[j - 1] + (left[i - 1] === right[j - 1] ? 0 : 1)
      );
    }
    previous = current;
  }
  return previous[right.length];
}

// Score 0..1 : similarité d'édition sur le nom complet + recouvrement de
// tokens, pour attraper aussi bien « recal » → « recall » que
// « memory_search » → « search_memory ».
function nameSimilarity(query, candidate) {
  const normalizedQuery = query.toLowerCase();
  const normalizedCandidate = candidate.toLowerCase();
  const editScore =
    1 -
    levenshtein(normalizedQuery, normalizedCandidate) /
      Math.max(normalizedQuery.length, normalizedCandidate.length, 1);

  const queryTokens = tokenize(query);
  const candidateTokens = tokenize(candidate);
  let overlap = 0;
  for (const token of queryTokens) {
    if (
      candidateTokens.some(
        (other) =>
          other === token ||
          other.startsWith(token) ||
          token.startsWith(other) ||
          levenshtein(token, other) <= 1
      )
    ) {
      overlap += 1;
    }
  }
  const tokenScore =
    overlap / Math.max(queryTokens.length, candidateTokens.length, 1);
  return Math.max(editScore, tokenScore * 0.9);
}

function shortDescription(description) {
  const text = (description ?? '').replace(/\s+/g, ' ').trim();
  return text.length > 120 ? `${text.slice(0, 120)}…` : text;
}

function rankTools(candidates, query, visible) {
  return candidates
    .filter((item) => visible(item.server))
    .map((item) => ({ ...item, score: nameSimilarity(query, item.tool) }))
    .filter((item) => item.score >= 0.4)
    .sort((left, right) => right.score - left.score)
    .slice(0, unknownSuggestionCount)
    .map(({ server, tool, description }) => ({ server, tool, description }));
}

async function withUnknownSuggestions(result, { server, tool, visible }) {
  if (!result.isError) return result;
  const text = extractText(result);
  const unknownTool = unknownToolPattern.test(text);
  const unknownServer = unknownServerPattern.test(text);
  if (!unknownTool && !unknownServer) return result;

  try {
    const servers = await readSchemaServers();
    const payload = { error: text };

    if (unknownTool) {
      payload.suggestions = rankTools(
        (servers[server] ?? []).map((item) => ({
          server,
          tool: item.name,
          description: shortDescription(item.description)
        })),
        tool,
        visible
      );
    } else {
      payload.similarServers = Object.keys(servers)
        .filter(visible)
        .map((name) => ({ server: name, score: nameSimilarity(server, name) }))
        .filter((item) => item.score >= 0.4)
        .sort((left, right) => right.score - left.score)
        .slice(0, unknownSuggestionCount)
        .map((item) => item.server);
      payload.suggestions = rankTools(
        Object.entries(servers).flatMap(([name, tools]) =>
          (tools ?? []).map((item) => ({
            server: name,
            tool: item.name,
            description: shortDescription(item.description)
          }))
        ),
        tool,
        visible
      );
    }

    payload.hint =
      payload.suggestions.length > 0
        ? 'Retry mcphub_call with one of these tools. Use mcphub_search with expand=true if you need the input schema.'
        : 'No similar tool found. Use mcphub_search with a task description.';

    usage.suggestionsServed += 1;
    markUsageDirty();
    return textResult(payload, true);
  } catch {
    return result;
  }
}

// ---------------------------------------------------------------------------
// État du daemon
// ---------------------------------------------------------------------------

async function readDaemonStatus() {
  const token = await readAuthToken();
  const response = await fetch(daemonApiUrl, {
    headers: { Authorization: `Bearer ${token}` },
    signal: AbortSignal.timeout(daemonProbeTimeoutMs)
  });
  if (!response.ok) {
    throw new Error(`McpHub dashboard returned HTTP ${response.status}.`);
  }
  const payload = await response.json();
  return {
    transport: hubConnection?.transport ?? 'not connected',
    cacheExists: payload.cacheExists,
    totalTools: payload.totalTools,
    settings: payload.settings,
    securityAlerts: payload.securityAlerts ?? [],
    servers: (payload.servers ?? []).map((server) => ({
      name: server.name,
      status: server.status,
      tools: server.tools,
      auth:
        /unauthorized|authentication|oauth|token/i.test(server.error || '')
          ? 'required'
          : server.status === 'error'
            ? 'error'
            : 'unknown',
      ...(server.error ? { error: server.error } : {})
    }))
  };
}

// ---------------------------------------------------------------------------
// Serveur MCP exposé à Cursor
// ---------------------------------------------------------------------------

const server = new McpServer(
  {
    name: 'McpHub',
    version: '1.4.0'
  },
  {
    instructions:
      'On mcphub_call and on each call of mcphub_batch, pass fields (keys to keep in each row) and max_rows whenever you expect a list (rows, logs, issues, invoices): the result comes back several times smaller. When a task needs three or more tool calls and you do not have to read a result to decide the next call (loops, filters, lookups feeding each other), send one mcphub_run script instead of separate calls: one round trip, and only its return value comes back.'
  }
);

// Les 7 outils réellement enregistrés, pour mesurer ce que McpHub coûte
// dans le catalogue du client (mcphub_savings).
const hubTools = new Map();
function registerHubTool(name, config, handler) {
  hubTools.set(name, config);
  return server.registerTool(name, config, async (...handlerArgs) => {
    await refreshProjects();
    return answerInTime(name, handler(...handlerArgs));
  });
}

// ---------------------------------------------------------------------------
// Appels longs : l'app Claude coupe un appel d'outil resté 60 s sans réponse
// (« Request timed out », mesuré le 2026-11-25 ; elle ne fournit pas de jeton de
// progression, donc rien ne repousse ce délai), et a déjà marqué McpHub
// indisponible après une telle coupure. On répond donc avant : passé
// answerBeforeMs, l'appel continue ici et son résultat se lit par poignée
// (server "mcphub", tool "call_result").
// ---------------------------------------------------------------------------

const pendingCalls = new Map();
let pendingCallCount = 0;

function waitFor(outcome, delayMs) {
  let timer;
  const late = new Promise((resolve) => {
    timer = setTimeout(() => resolve(undefined), delayMs);
  });
  return Promise.race([outcome, late]).finally(() => clearTimeout(timer));
}

function deliver(settled) {
  if (settled.error) throw settled.error;
  return settled.value;
}

function pendingNotice(handle, { name, startedAt }) {
  const seconds = Math.round((Date.now() - startedAt) / 1000);
  return {
    ...textResult(
      `Still running after ${seconds} s: ${name} continues in the background. Do not call it again, it may already have had its effect. Get its result with mcphub_call server "mcphub", tool "call_result", arguments {"handle":"${handle}"}.`
    ),
    _meta: { mcphub: { status: 'pending', handle } }
  };
}

async function answerInTime(name, work) {
  const startedAt = Date.now();
  // Jamais de rejet non géré quand l'appel se termine après la réponse d'attente.
  const pending = { name, startedAt, finished: false };
  pending.outcome = Promise.resolve(work)
    .then(
      (value) => ({ value }),
      (error) => ({ error })
    )
    .finally(() => {
      pending.finished = true;
    });
  const settled = await waitFor(pending.outcome, answerBeforeMs);
  if (settled) return deliver(settled);
  pendingCallCount += 1;
  const handle = `c${pendingCallCount}`;
  pendingCalls.set(handle, pending);
  // Seuls des résultats terminés et jamais lus sont oubliés : un appel encore en
  // cours garde sa poignée.
  for (const [oldHandle, oldCall] of pendingCalls) {
    if (pendingCalls.size <= pendingCallLimit) break;
    if (oldCall.finished) pendingCalls.delete(oldHandle);
  }
  return pendingNotice(handle, pending);
}

async function readPendingCall({ handle }) {
  const pending = typeof handle === 'string' ? pendingCalls.get(handle) : undefined;
  if (!pending) {
    return textResult(
      `No pending call for handle "${handle}": its result was already read, or this McpHub client restarted since. Check the effect of the call before running it again.`,
      true
    );
  }
  // Plus court que answerBeforeMs : cette lecture passe elle-même par answerInTime.
  const settled = await waitFor(pending.outcome, Math.round(answerBeforeMs * 0.8));
  if (!settled) return pendingNotice(handle, pending);
  pendingCalls.delete(handle);
  return deliver(settled);
}

const projectFields = {
  project: z.string().optional().describe('Persistent project alias.'),
  working_dir: z
    .string()
    .optional()
    .describe('Workspace path used for automatic project routing.')
};

registerHubTool(
  'mcphub_bootstrap',
  {
    description:
      'Call once per conversation. Resolves project aliases and permissions, then recalls compact MemoryPilot context (in Claude Code, when the MemoryPilot SessionStart hook injected it in the last 10 minutes, only the episodes matching hints). This is the only tool that triggers MemoryPilot implicitly. When the result has securityAlerts, tell the user which tools McpHub blocked and why.',
    inputSchema: {
      ...projectFields,
      hints: z.string().optional().describe('Current task keywords.')
    }
  },
  async ({ project, working_dir, hints }) => {
    const [projectName, projectConfig] = resolveProject(project, working_dir);
    // Le dossier de travail réel d'abord : un projet à plusieurs workspaces ne doit
    // pas rappeler la mémoire du premier quand on travaille dans un autre.
    const candidate = working_dir ? normalize(working_dir) : undefined;
    const workspace =
      (candidate &&
        projectConfig?.workspaces.find((root) => insideWorkspace(candidate, root))) ??
      working_dir ??
      projectConfig?.workspaces[0];
    if (!workspace) {
      return textResult(
        { project: null, error: 'working_dir is required when no McpHub project mapping matches.' },
        true
      );
    }
    usage.bootstrapCalls += 1;
    markUsageDirty();
    const injection = await recentHookInjection(workspace);
    const [context, securityAlerts] = await Promise.all([
      injection
        ? recallEpisodes(projectName, workspace, hints).then((episodes) => ({
            memory: `Already in context: injected at ${injection.injectedAt} by the MemoryPilot SessionStart hook. Only episodes matching the hints follow. For a targeted lookup, call mcphub_call with server MemoryPilot, tool recall and hints.`,
            ...episodes
          }))
        : recallContext(projectName, workspace, hints),
      readDaemonStatus()
        .then((s) => s.securityAlerts)
        .catch(() => [])
    ]);
    return textResult({
      ...(securityAlerts.length
        ? {
            securityAlerts,
            securityNotice:
              'McpHub has blocked these tools. Tell the user now, in one or two sentences: which server and tool, and why. Do not approve them yourself; the user decides after review.'
          }
        : {}),
      project: projectName,
      workspace,
      ...(projectConfig
        ? {
            aliases: projectConfig.aliases,
            ...(projectConfig.tenant
              ? {
                  tenant: projectConfig.tenant,
                  tenantServers: Object.keys(projectConfig.credentials ?? {}),
                  sharedServers: projectConfig.sharedServers ?? [],
                  tenantNote:
                    'Client accounts: only tenantServers (with the client tokens) and sharedServers answer from this folder. Always pass working_dir.'
                }
              : { allowedServers: projectConfig.allowedServers }),
            ...(projectConfig.ownedServers?.length ? { ownedServers: projectConfig.ownedServers } : {})
          }
        : { policy: 'none: no McpHub project mapping for this folder, every server is reachable' }),
      ...context
    });
  }
);

registerHubTool(
  'mcphub_search',
  {
    description:
      'Search child MCP tools (BM25 over the persistent schema cache). Only needed when the server or tool name is unknown: mcphub_call already returns suggestions on unknown tools and the input schema on argument errors. Project permissions are applied automatically.',
    inputSchema: {
      query: z.string(),
      server: z.string().optional(),
      expand: z.boolean().optional().default(false),
      top_k: z.number().int().min(1).max(50).optional(),
      offset: z.number().int().min(0).optional(),
      ...projectFields
    }
  },
  async ({ project, working_dir, ...args }) => {
    const [projectName, projectConfig] = resolveProject(project, working_dir);
    if (args.server && !ownerAllows(args.server, working_dir)) {
      return textResult(
        `Server "${args.server}" belongs to project "${serverOwners.get(args.server)}": it only answers when working_dir is inside that project's workspace.`,
        true
      );
    }
    if (
      projectConfig &&
      args.server &&
      !projectAllows(projectConfig, args.server)
    ) {
      return textResult(
        `Server "${args.server}" is not allowed for project "${projectName}".`,
        true
      );
    }
    usage.searches += 1;
    markUsageDirty();
    try {
      await assertTenantSupport(working_dir);
      const result = await callHubTool('mcphub_search', { ...args, ...hubContext(working_dir) }, { replaySafe: true });
      return filterSearchResult(result, projectName, projectConfig, working_dir);
    } catch (error) {
      return textResult(errorMessage(error), true);
    }
  }
);

registerHubTool(
  'mcphub_call',
  {
    description:
      'Call one child MCP tool directly with project-level resource and server guards. Unknown tool or server names return the closest matching tools; wrong arguments return the input schema. For a list result, pass fields and max_rows to keep it small.',
    inputSchema: {
      server: z.string(),
      tool: z.string(),
      arguments: z.record(z.string(), z.unknown()).optional(),
      bypass_cache: z.boolean().optional(),
      confirmation_token: z
        .string()
        .optional()
        .describe('Token returned by a CONFIRMATION REQUISE answer, only after the user explicitly agreed.'),
      ...shapingFields,
      ...projectFields
    }
  },
  async ({
    server: childServer,
    tool,
    arguments: args = {},
    bypass_cache,
    confirmation_token,
    project,
    working_dir,
    ...shaping
  }) => {
    try {
      if (childServer === hubLocalServer && tool === 'read_result') return readStoredResult(args, working_dir);
      if (childServer === hubLocalServer && tool === 'search_result') return searchStoredResult(args, working_dir);
      if (childServer === hubLocalServer && tool === 'call_result') return await readPendingCall(args);
      const [projectName, projectConfig] = resolveProject(project, working_dir);
      const securedArgs = enforceProjectPolicy({
        projectName,
        projectConfig,
        server: childServer,
        tool,
        args,
        workingDirectory: working_dir
      });
      usage.calls += 1;
      markUsageDirty();
      const result = await callChildWithSuggestions({
        server: childServer,
        tool,
        args: securedArgs,
        bypass_cache,
        confirmation_token,
        visible: serverVisibility(projectConfig, working_dir),
        workingDirectory: working_dir
      });
      const shaped = shapeChildResult(result, pickShaping(shaping), childServer).result;
      recordServerUsage(childServer, shaped);
      return shaped;
    } catch (error) {
      return errorToResult(error);
    }
  }
);

registerHubTool(
  'mcphub_batch',
  {
    description:
      'Call up to 20 independent child MCP tools in parallel in one model round-trip.',
    inputSchema: {
      calls: z
        .array(
          z.object({
            id: z.string(),
            server: z.string(),
            tool: z.string(),
            arguments: z.record(z.string(), z.unknown()).optional(),
            bypass_cache: z.boolean().optional(),
            ...shapingFields
          })
        )
        .min(1)
        .max(20),
      ...projectFields
    }
  },
  async ({ calls, project, working_dir }) => {
    const [projectName, projectConfig] = resolveProject(project, working_dir);
    usage.batches += 1;
    usage.calls += calls.length;
    markUsageDirty();

    const results = await Promise.all(
      calls.map(async (call) => {
        try {
          const securedArgs = enforceProjectPolicy({
            projectName,
            projectConfig,
            server: call.server,
            tool: call.tool,
            args: call.arguments ?? {},
            workingDirectory: working_dir
          });
          const result = await callChildWithSuggestions({
            server: call.server,
            tool: call.tool,
            args: securedArgs,
            bypass_cache: call.bypass_cache,
            visible: serverVisibility(projectConfig, working_dir),
            workingDirectory: working_dir
          });
          const status = callStatus(result);
          const shaped = shapeChildResult(result, pickShaping(call), call.server);
          recordServerUsage(call.server, shaped.value);
          return {
            id: call.id,
            ok: status === 'ok',
            status,
            result: shaped.value,
            ...(shaped.notes.length ? { notes: shaped.notes } : {}),
            ...(result.structuredContent ? { structuredContent: result.structuredContent } : {}),
            ...(status === 'confirmation_required'
              ? {
                  confirmation_token:
                    result._meta?.mcphub?.confirmation_token ?? result._mcphub?.confirmation_token
                }
              : {})
          };
        } catch (error) {
          return {
            id: call.id,
            ok: false,
            status: error?.status ?? error?.data?.status ?? 'error',
            error: errorMessage(error)
          };
        }
      })
    );
    return textResult({ project: projectName, results });
  }
);

registerHubTool(
  'mcphub_run',
  {
    description:
      'Run JavaScript that chains several child MCP tool calls in a sandbox and returns only the final value: use it for multi-step or bulk work instead of many mcphub_call round-trips. Inside the code: `await mcp.call(server, tool, args)` returns the tool result, `await mcp.search(query, server?)` finds tools, console.log is captured, `return` gives the output. No filesystem, network or require. Project permissions apply to every call.',
    inputSchema: {
      code: z
        .string()
        .describe(
          "Body of an async function, e.g. const p = await mcp.call('supabase','list_projects',{}); return p.length;"
        ),
      timeout_secs: z.number().int().min(5).max(300).optional(),
      max_chars: shapingFields.max_chars,
      ...projectFields
    }
  },
  async ({ code, timeout_secs, max_chars, project, working_dir }) => {
    try {
      const [, projectConfig] = resolveProject(project, working_dir);
      const policy = runPolicyFor(projectConfig, working_dir);
      if (policy && !(await daemonEnforcesRunPolicy())) {
        return textResult(
          'mcphub_run is unavailable here: this folder has project rules and the running McpHub daemon is too old to enforce them inside a script. Update McpHub, or use mcphub_call and mcphub_batch.',
          true
        );
      }
      usage.runs += 1;
      markUsageDirty();
      await assertTenantSupport(working_dir);
      const result = await callHubTool('mcphub_run', {
        ...hubContext(working_dir),
        code,
        ...(timeout_secs ? { timeout_secs } : {}),
        ...(policy ? { policy } : {})
      });
      // Le texte d'un très gros résultat est coupé par le moteur et ne se parse plus :
      // la mention se lit alors dans le texte lui-même.
      const runText = extractText(result);
      const runPayload = parseJsonContainer(runText);
      const policyConfirmed = runPayload?.policy === 'enforced' || /"policy":\s*"enforced"/.test(runText);
      // Appels faits par le script : autant d'allers-retours que l'agent n'a pas eu à faire.
      const scriptCalls = runPayload?.calls ?? Number(/"calls":\s*(\d+)/.exec(runText)?.[1] ?? 0);
      if (!result.isError && Number.isInteger(scriptCalls)) {
        usage.runCalls += scriptCalls;
        markUsageDirty();
      }
      if (policy && !result.isError && !policyConfirmed) {
        return textResult(
          'mcphub_run ran but the daemon did not confirm that the project rules were enforced: result withheld. Update McpHub.',
          true
        );
      }
      return shapeChildResult(result, { max_chars }, hubLocalServer).result;
    } catch (error) {
      return errorToResult(error);
    }
  }
);

registerHubTool(
  'mcphub_status',
  {
    description:
      'Show redacted child-server health, authentication state, the active hub transport and securityAlerts (tools the guard blocked, with the reason). Never returns commands, arguments, environment variables, or secrets.',
    inputSchema: {
      server: z.string().optional(),
      working_dir: projectFields.working_dir
    }
  },
  async ({ server: serverFilter, working_dir }) => {
    try {
      const status = await readDaemonStatus();
      status.servers = status.servers.filter(
        (child) =>
          (!serverFilter || child.name === serverFilter) &&
          ownerAllows(child.name, working_dir)
      );
      return textResult(status);
    } catch (error) {
      return textResult(errorMessage(error), true);
    }
  }
);

// ---------------------------------------------------------------------------
// mcphub_savings : comparaison honnête.
//
// Depuis janvier 2026 Cursor ne charge plus les inputSchemas en eager : seul
// un catalogue (nom + description tronquée à 200 caractères) entre dans le
// contexte, les schémas sont récupérés à la demande. L'économie réelle du hub
// côté tokens est donc « catalogue de N outils » vs « catalogue de 6 outils »,
// pas « tous les schémas » vs « 6 outils ». Le gain principal est ailleurs :
// un seul jeu de processus enfants partagé entre fenêtres Cursor.
//
// Claude Code va plus loin : ses outils différés n'entrent dans le contexte que
// par leur nom (mcp__<serveur>__<outil>), sans description, ceux de McpHub
// compris. La comparaison honnête y est donc noms contre noms.
// ---------------------------------------------------------------------------

function measureHubCatalog() {
  let fullBytes = 0;
  let nameBytes = 0;
  for (const [name, config] of hubTools) {
    fullBytes += Buffer.byteLength(
      JSON.stringify({
        name,
        description: config.description,
        inputSchema: z.toJSONSchema(z.object(config.inputSchema ?? {}))
      })
    );
    nameBytes += Buffer.byteLength(`mcp__${hubServerName}__${name}\n`);
  }
  return { fullBytes, nameBytes };
}

async function readSchemaCatalog() {
  const servers = await readSchemaServers();
  let toolCount = 0;
  let fullSchemaBytes = 0;
  let cursorCatalogBytes = 0;
  let claudeCodeNameBytes = 0;

  for (const [serverName, tools] of Object.entries(servers)) {
    for (const tool of tools ?? []) {
      toolCount += 1;
      claudeCodeNameBytes += Buffer.byteLength(`mcp__${serverName}__${tool.name ?? ''}\n`);
      fullSchemaBytes += Buffer.byteLength(JSON.stringify(tool));
      const description = tool.description ?? '';
      cursorCatalogBytes +=
        Buffer.byteLength(tool.name ?? '') +
        Buffer.byteLength(
          description.length > cursorCatalogDescriptionLimit
            ? `${description.slice(0, cursorCatalogDescriptionLimit)}... [truncated]`
            : description
        ) +
        24;
    }
  }

  return {
    serverCount: Object.keys(servers).length,
    toolCount,
    fullSchemaBytes,
    cursorCatalogBytes,
    claudeCodeNameBytes
  };
}

registerHubTool(
  'mcphub_savings',
  {
    description:
      'Report McpHub call statistics and an honest context-size comparison against native deferred tool loading (Cursor, and Claude Code when it is the client).',
    inputSchema: {}
  },
  async () => {
    try {
      const catalog = await readSchemaCatalog();
      const sharedUsage = await readSharedUsage();
      const { byServer: serverUsage, ...totalUsage } = sharedUsage;
      const catalogSaved = Math.max(
        0,
        catalog.cursorCatalogBytes - hubCatalog.fullBytes
      );
      const claudeCodeSaved = Math.max(
        0,
        catalog.claudeCodeNameBytes - hubCatalog.nameBytes
      );
      const versusClaudeCodeDeferredLoading = {
        directCatalogBytes: catalog.claudeCodeNameBytes,
        hubCatalogBytes: hubCatalog.nameBytes,
        savedBytes: claudeCodeSaved,
        approximateTokensSavedPerConversation: Math.round(claudeCodeSaved / 4),
        methodology:
          'Names against names: Claude Code lists deferred tools as mcp__<server>__<tool> and loads schemas on demand, for the direct catalog as for the 7 McpHub tools. Not counted: usage costs on either side (schemas loaded on demand, mcphub_search results, extra round trips). Bytes divided by four; excludes prompts and tool results.'
      };
      const legacySaved = Math.max(
        0,
        catalog.fullSchemaBytes - hubCatalog.fullBytes
      );
      return textResult({
        client: server.server.getClientVersion()?.name ?? 'unknown',
        ...(isClaudeCodeClient() ? { versusClaudeCodeDeferredLoading } : {}),
        catalog: {
          servers: catalog.serverCount,
          tools: catalog.toolCount,
          hubExposedBytes: hubCatalog.fullBytes
        },
        versusCursorDeferredLoading: {
          directCatalogBytes: catalog.cursorCatalogBytes,
          savedBytes: catalogSaved,
          savedPercent: Number(
            ((catalogSaved / Math.max(1, catalog.cursorCatalogBytes)) * 100).toFixed(1)
          ),
          approximateTokensSavedPerConversation: Math.round(catalogSaved / 4),
          methodology:
            'Cursor loads tool names plus descriptions truncated to 200 chars and fetches schemas on demand. Bytes divided by four; excludes prompts and tool results.'
        },
        versusLegacyEagerLoading: {
          directSchemaBytes: catalog.fullSchemaBytes,
          savedBytes: legacySaved,
          approximateTokensSaved: Math.round(legacySaved / 4),
          methodology:
            'Only relevant for clients that still inject every inputSchema up front. Cursor is not one of them since January 2026.'
        },
        process: {
          transport: hubConnection?.transport ?? 'not connected',
          note:
            hubConnection?.transport === 'daemon'
              ? 'Child servers are shared with every Cursor window through the McpHub daemon.'
              : 'Child servers are spawned per Cursor window. Start the daemon (McpHub install) to share them.'
        },
        codeMode: {
          runs: sharedUsage.runs,
          callsInsideScripts: sharedUsage.runCalls,
          roundTripsSaved: Math.max(0, sharedUsage.runCalls - sharedUsage.runs),
          note: 'Each mcphub_run is one round trip, whatever the number of tool calls its script makes.'
        },
        perServer: {
          servers: Object.entries(serverUsage)
            .map(([server, { calls, resultBytes }]) => ({
              server,
              calls,
              resultBytes,
              approximateTokens: Math.round(resultBytes / 4)
            }))
            .sort((left, right) => right.resultBytes - left.resultBytes),
          note: 'Result bytes delivered to the agent by mcphub_call and mcphub_batch, after shaping, counted since this field was added. Calls made inside mcphub_run scripts are not attributed to a server. Bytes divided by four.'
        },
        actualUsage: totalUsage
      });
    } catch (error) {
      return textResult(errorMessage(error), true);
    }
  }
);

// Mesuré une fois, sur les outils tels que tools/list les décrit.
const hubCatalog = measureHubCatalog();

const transport = new StdioServerTransport();
await server.connect(transport);
