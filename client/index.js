#!/usr/bin/env node

import { readFile, stat, writeFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join, normalize, sep } from 'node:path';

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { SSEClientTransport } from '@modelcontextprotocol/sdk/client/sse.js';
import {
  getDefaultEnvironment,
  StdioClientTransport
} from '@modelcontextprotocol/sdk/client/stdio.js';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { ErrorCode, McpError } from '@modelcontextprotocol/sdk/types.js';
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
const schemaCachePath = join(hubDirectory, 'schema-cache.json');
const authTokenPath = join(hubDirectory, 'auth-token');
const hubBinaryPath = join(homedir(), '.local', 'bin', 'McpHub');
const daemonBaseUrl =
  process.env.MCPHUB_DAEMON_URL ?? `http://127.0.0.1:${process.env.MCPHUB_PORT || 24680}`;
const daemonSseUrl = new URL('/sse', daemonBaseUrl);
const daemonApiUrl = new URL('/api/servers', daemonBaseUrl);

// Cursor limite un appel d'outil à quelques minutes ; on laisse une marge large
// mais bornée pour ne jamais laisser une requête pendre indéfiniment.
const childCallTimeoutMs = Number(process.env.MCPHUB_CALL_TIMEOUT_MS ?? 600_000);
const daemonProbeTimeoutMs = 1_500;
const usageFlushDelayMs = 2_000;
const unknownSuggestionCount = 5;

// Nombre d'octets exposés par le client McpHub lui-même dans le catalogue Cursor
// (6 outils, noms + descriptions + schémas). Mesuré via tools/list.
const hubExposedCatalogBytes = 3_600;
const cursorCatalogDescriptionLimit = 200;

const projectsConfig = await readProjects(projectsPath);
const serverOwners = buildServerOwners(projectsConfig.projects);

/** @type {{ client: Client, transport: 'daemon' | 'stdio' } | undefined} */
let hubConnection;
/** @type {Promise<{ client: Client, transport: 'daemon' | 'stdio' }> | undefined} */
let hubConnecting;
/** @type {Map<string, Promise<{ memory?: string, memoryError?: string }>>} */
const pendingBootstraps = new Map();

let usage = await readJson(usagePath, {
  calls: 0,
  searches: 0,
  batches: 0,
  bootstrapCalls: 0,
  suggestionsServed: 0,
  childResultBytes: 0,
  startedAt: new Date().toISOString()
});
usage.suggestionsServed ??= 0;

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

function flushUsage() {
  if (!usageDirty) return usageFlushing;
  usageDirty = false;
  usageFlushing = writeFile(usagePath, `${JSON.stringify(usage, null, 2)}\n`, {
    mode: 0o600
  }).catch(() => {
    // Les statistiques ne doivent jamais faire échouer un appel d'outil.
  });
  return usageFlushing;
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
  return new Client({ name: 'mcphub-client', version: '1.1.0' });
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
// Seules les coupures de transport et les timeouts justifient un retry.
function isTransportFailure(error) {
  if (!(error instanceof McpError)) return true;
  return (
    error.code === ErrorCode.ConnectionClosed ||
    error.code === ErrorCode.RequestTimeout
  );
}

async function callHubTool(name, args, retry = true) {
  try {
    const { client } = await connectHub();
    const result = await client.callTool(
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
    if (!retry || !isTransportFailure(error)) throw error;
    await resetHubConnection();
    return callHubTool(name, args, false);
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

function allowedServersOf(projectConfig) {
  return new Set([
    ...(projectConfig.allowedServers ?? []),
    ...(projectConfig.ownedServers ?? [])
  ]);
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
  const allowed = projectConfig ? allowedServersOf(projectConfig) : null;
  return (server) =>
    (!allowed || allowed.has(server)) && ownerAllows(server, workingDirectory);
}

function enforceProjectPolicy({ projectName, projectConfig, server, tool, args, workingDirectory }) {
  assertOwnership(server, workingDirectory);
  if (!projectConfig) return { ...args };
  if (!allowedServersOf(projectConfig).has(server)) {
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
      `Supabase project mismatch for ${projectName}: expected ${aliases.supabaseProjectId}.`
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

// Le recall MemoryPilot n'est déclenché que par mcphub_bootstrap : il ne doit
// jamais retarder un appel Supabase ou Cloudflare qui n'en a pas besoin.
// Le processus vit aussi longtemps que la fenêtre Cursor, donc sur plusieurs
// conversations : seuls les appels simultanés identiques partagent un recall.
function recallContext(projectName, workspace, hints) {
  const key = JSON.stringify([projectName, workspace, hints || '']);
  const pending = pendingBootstraps.get(key);
  if (pending) return pending;

  const recall = callHubTool('mcphub_call', {
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
  })
    .then((result) => ({ memory: extractText(result) }))
    .catch((error) => ({ memoryError: errorMessage(error) }))
    .finally(() => pendingBootstraps.delete(key));

  pendingBootstraps.set(key, recall);
  return recall;
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

function errorToResult(error) {
  return textResult(errorMessage(error), true);
}

async function callChildWithSuggestions({ server, tool, args, bypass_cache, visible }) {
  let result;
  try {
    result = await callHubTool('mcphub_call', {
      server,
      tool,
      arguments: args,
      bypass_cache
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
    .toLowerCase()
    .replace(/([a-z])([A-Z])/g, '$1 $2')
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

const server = new McpServer({
  name: 'McpHub',
  version: '1.1.0'
});

const projectFields = {
  project: z.string().optional().describe('Persistent project alias.'),
  working_dir: z
    .string()
    .optional()
    .describe('Workspace path used for automatic project routing.')
};

server.registerTool(
  'mcphub_bootstrap',
  {
    description:
      'Call once per conversation. Resolves project aliases and permissions, then recalls compact MemoryPilot context. This is the only tool that triggers MemoryPilot implicitly.',
    inputSchema: {
      ...projectFields,
      hints: z.string().optional().describe('Current task keywords.')
    }
  },
  async ({ project, working_dir, hints }) => {
    const [projectName, projectConfig] = resolveProject(project, working_dir);
    const workspace = projectConfig?.workspaces[0] ?? working_dir;
    if (!workspace) {
      return textResult(
        { project: null, error: 'working_dir is required when no McpHub project mapping matches.' },
        true
      );
    }
    usage.bootstrapCalls += 1;
    markUsageDirty();
    const context = await recallContext(projectName, workspace, hints);
    return textResult({
      project: projectName,
      workspace,
      ...(projectConfig
        ? {
            aliases: projectConfig.aliases,
            allowedServers: projectConfig.allowedServers,
            ...(projectConfig.ownedServers?.length ? { ownedServers: projectConfig.ownedServers } : {})
          }
        : { policy: 'none: no McpHub project mapping for this folder, every server is reachable' }),
      ...context
    });
  }
);

server.registerTool(
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
      !allowedServersOf(projectConfig).has(args.server)
    ) {
      return textResult(
        `Server "${args.server}" is not allowed for project "${projectName}".`,
        true
      );
    }
    usage.searches += 1;
    markUsageDirty();
    try {
      const result = await callHubTool('mcphub_search', args);
      return filterSearchResult(result, projectName, projectConfig, working_dir);
    } catch (error) {
      return textResult(errorMessage(error), true);
    }
  }
);

server.registerTool(
  'mcphub_call',
  {
    description:
      'Call one child MCP tool directly with project-level resource and server guards. Unknown tool or server names return the closest matching tools; wrong arguments return the input schema.',
    inputSchema: {
      server: z.string(),
      tool: z.string(),
      arguments: z.record(z.string(), z.unknown()).optional(),
      bypass_cache: z.boolean().optional(),
      ...projectFields
    }
  },
  async ({
    server: childServer,
    tool,
    arguments: args = {},
    bypass_cache,
    project,
    working_dir
  }) => {
    try {
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
      return await callChildWithSuggestions({
        server: childServer,
        tool,
        args: securedArgs,
        bypass_cache,
        visible: serverVisibility(projectConfig, working_dir)
      });
    } catch (error) {
      return textResult(errorMessage(error), true);
    }
  }
);

server.registerTool(
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
            bypass_cache: z.boolean().optional()
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
            visible: serverVisibility(projectConfig, working_dir)
          });
          return {
            id: call.id,
            ok: !result.isError,
            result: extractText(result)
          };
        } catch (error) {
          return {
            id: call.id,
            ok: false,
            error: errorMessage(error)
          };
        }
      })
    );
    return textResult({ project: projectName, results });
  }
);

server.registerTool(
  'mcphub_status',
  {
    description:
      'Show redacted child-server health, authentication state and the active hub transport. Never returns commands, arguments, environment variables, or secrets.',
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
// ---------------------------------------------------------------------------

async function readSchemaCatalog() {
  const servers = await readSchemaServers();
  let toolCount = 0;
  let fullSchemaBytes = 0;
  let cursorCatalogBytes = 0;

  for (const tools of Object.values(servers)) {
    for (const tool of tools ?? []) {
      toolCount += 1;
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
    cursorCatalogBytes
  };
}

server.registerTool(
  'mcphub_savings',
  {
    description:
      'Report McpHub call statistics and an honest context-size comparison against Cursor native deferred tool loading.',
    inputSchema: {}
  },
  async () => {
    try {
      const catalog = await readSchemaCatalog();
      const catalogSaved = Math.max(
        0,
        catalog.cursorCatalogBytes - hubExposedCatalogBytes
      );
      const legacySaved = Math.max(
        0,
        catalog.fullSchemaBytes - hubExposedCatalogBytes
      );
      return textResult({
        catalog: {
          servers: catalog.serverCount,
          tools: catalog.toolCount,
          hubExposedBytes: hubExposedCatalogBytes
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
        actualUsage: usage
      });
    } catch (error) {
      return textResult(errorMessage(error), true);
    }
  }
);

const transport = new StdioServerTransport();
await server.connect(transport);
