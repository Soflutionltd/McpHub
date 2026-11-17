// Offline tests: index.js against a fake McpHub daemon (SSE), no real servers,
// no user files. Run with `npm run test:offline`.
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { SSEServerTransport } from '@modelcontextprotocol/sdk/server/sse.js';
import { z } from 'zod';

const token = 'mcphub_test_token';
const received = [];

function fakeHub() {
  const hub = new McpServer({ name: 'fake-hub', version: '1.0.0' });
  hub.registerTool(
    'mcphub_call',
    {
      inputSchema: {
        server: z.string(),
        tool: z.string(),
        arguments: z.record(z.string(), z.unknown()).optional(),
        bypass_cache: z.boolean().optional(),
        confirmation_token: z.string().optional()
      }
    },
    async (args) => {
      received.push(args);
      if (args.tool === 'send_email') return new Promise(() => {});
      if (args.tool === 'recall' && args.arguments?.episodes_only) {
        return { content: [{ type: 'text', text: JSON.stringify({ episodes: [{ id: 'ep-1' }] }) }] };
      }
      if (args.tool === 'delete_row' && !args.confirmation_token) {
        return {
          content: [{ type: 'text', text: 'CONFIRMATION REQUISE' }],
          isError: false,
          _meta: { mcphub: { status: 'confirmation_required', confirmation_token: 'tok-1' } }
        };
      }
      if (args.tool === 'list_rows') {
        const rows = [
          { id: 1, name: 'alpha', deleted_at: null, note: '' },
          { id: 2, name: 'beta', deleted_at: null, note: 'kept' },
          { id: 3, name: 'gamma', deleted_at: null, note: '' }
        ];
        return { content: [{ type: 'text', text: JSON.stringify({ rows, cursor: null, tags: [] }, null, 2) }] };
      }
      if (args.tool === 'big_text') {
        return { content: [{ type: 'text', text: `${'a'.repeat(1500)}${'b'.repeat(1500)}` }] };
      }
      if (args.tool === 'fixed_text') {
        return { content: [{ type: 'text', text: String(args.arguments.text) }] };
      }
      if (args.tool === 'big_id') {
        return { content: [{ type: 'text', text: '{"id": 12345678901234567890, "note": null}' }] };
      }
      return { content: [{ type: 'text', text: `done ${args.tool}` }] };
    }
  );
  return hub;
}

const transports = new Map();
let daemonUp = true;
const http = createServer(async (req, res) => {
  if (req.headers.authorization !== `Bearer ${token}`) {
    res.writeHead(401).end();
    return;
  }
  const url = new URL(req.url, 'http://127.0.0.1');
  if (!daemonUp) {
    res.writeHead(503).end();
    return;
  }
  if (url.pathname === '/api/servers') {
    res.writeHead(200, { 'Content-Type': 'application/json' }).end('{"servers":[]}');
  } else if (url.pathname === '/sse') {
    const transport = new SSEServerTransport('/message', res);
    transports.set(transport.sessionId, transport);
    await fakeHub().connect(transport);
  } else if (url.pathname === '/message') {
    const transport = transports.get(url.searchParams.get('sessionId'));
    if (!transport) {
      res.writeHead(404).end();
      return;
    }
    await transport.handlePostMessage(req, res);
  } else {
    res.writeHead(404).end();
  }
});
await new Promise((resolve) => http.listen(0, '127.0.0.1', resolve));
const port = http.address().port;

const home = await mkdtemp(join(tmpdir(), 'mcphub-offline-'));
await writeFile(join(home, 'auth-token'), token);
await mkdir(join(home, 'memorypilot'));
const clientEnv = {
  MCPHUB_HOME: home,
  MCPHUB_DAEMON_URL: `http://127.0.0.1:${port}`,
  MEMORYPILOT_DB: join(home, 'memorypilot', 'memory.db')
};
await writeFile(join(home, 'projects.json'), '{"projects":{}}');
await writeFile(
  join(home, 'schema-cache.json'),
  JSON.stringify({ servers: { db: [{ name: 'read_row', description: 'Read one row.' }] } })
);

const client = new Client({ name: 'mcphub-offline-test', version: '1.0.0' });
await client.connect(
  new StdioClientTransport({
    command: process.execPath,
    args: [process.env.MCPHUB_CLIENT_ENTRY ?? 'index.js'],
    cwd: new URL('.', import.meta.url).pathname,
    env: {
      ...process.env,
      ...clientEnv,
      MCPHUB_CALL_TIMEOUT_MS: '1500'
    },
    stderr: 'pipe'
  })
);

function text(result) {
  return result.content[0].text;
}

try {
  // A mutation whose answer never comes is reported as unknown, never replayed.
  const lost = await client.callTool({
    name: 'mcphub_call',
    arguments: { server: 'mail', tool: 'send_email', arguments: { to: 'a@example.com' } }
  });
  assert.equal(lost.isError, true);
  assert.match(text(lost), /\[issue inconnue\]/);
  assert.equal(lost._meta?.mcphub?.status, 'unknown_outcome');
  assert.equal(received.filter((call) => call.tool === 'send_email').length, 1);

  // A confirmation request is not a success in a batch.
  const batch = await client.callTool({
    name: 'mcphub_batch',
    arguments: {
      calls: [
        { id: 'a', server: 'db', tool: 'delete_row', arguments: { id: 1 } },
        { id: 'b', server: 'db', tool: 'read_row', arguments: { id: 1 } }
      ]
    }
  });
  const results = JSON.parse(text(batch)).results;
  const confirm = results.find((item) => item.id === 'a');
  assert.equal(confirm.ok, false);
  assert.equal(confirm.status, 'confirmation_required');
  assert.equal(confirm.confirmation_token, 'tok-1');
  assert.deepEqual(
    results.find((item) => item.id === 'b'),
    { id: 'b', ok: true, status: 'ok', result: 'done read_row' }
  );

  // The confirmation token reaches the hub.
  const confirmed = await client.callTool({
    name: 'mcphub_call',
    arguments: { server: 'db', tool: 'delete_row', arguments: { id: 1 }, confirmation_token: 'tok-1' }
  });
  assert.equal(text(confirmed), 'done delete_row');
  assert.equal(received.at(-1).confirmation_token, 'tok-1');

  // A JSON result loses its null and empty-string fields and its indentation; empty arrays stay.
  const call = (tool, extra = {}) => ({
    name: 'mcphub_call',
    arguments: { server: 'db', tool, arguments: {}, ...extra }
  });
  const pruned = await client.callTool(call('list_rows'));
  assert.equal(
    text(pruned),
    '{"rows":[{"id":1,"name":"alpha"},{"id":2,"name":"beta","note":"kept"},{"id":3,"name":"gamma"}],"tags":[]}'
  );
  assert.equal(pruned.content.length, 1);

  // fields and max_rows shape the rows; the cut is announced apart from the JSON.
  const shaped = await client.callTool(call('list_rows', { fields: ['id'], max_rows: 2 }));
  assert.equal(text(shaped), '{"rows":[{"id":1},{"id":2}],"tags":[]}');
  assert.equal(shaped.content[1].text, '[McpHub] rows: kept 2 of 3 rows (max_rows).');

  // raw returns the child text untouched.
  const untouched = await client.callTool(call('list_rows', { raw: true }));
  assert.match(text(untouched), /"deleted_at": null/);

  // A number that would lose precision keeps the result unrewritten.
  assert.equal(text(await client.callTool(call('big_id'))), '{"id": 12345678901234567890, "note": null}');

  // An out-of-range exponent would come back as null: the result is left as it is.
  const fixed = (value, extra = {}) =>
    client.callTool({
      name: 'mcphub_call',
      arguments: { server: 'db', tool: 'fixed_text', arguments: { text: value }, ...extra }
    });
  assert.equal(text(await fixed('{"a":1e400,"b":null}')), '{"a":1e400,"b":null}');

  // A __proto__ key is data like any other.
  assert.equal(text(await fixed('{"__proto__":{"x":1},"b":2,"c":null}')), '{"__proto__":{"x":1},"b":2}');

  // fields on a result without rows is said, not silently ignored.
  const noRows = await fixed('{"id":1,"name":"alpha"}', { fields: ['id'] });
  assert.equal(text(noRows), '{"id":1,"name":"alpha"}');
  assert.match(noRows.content[1].text, /fields and max_rows ignored/);

  // A cut never splits a character, and a one-char read on an emoji still moves forward.
  const emoji = await fixed(`${'a'.repeat(999)}😀${'b'.repeat(10)}`, { max_chars: 1000 });
  assert.equal(text(emoji), 'a'.repeat(999));
  const emojiHandle = /"handle":"(r\d+)","offset":999/.exec(emoji.content[1].text)?.[1];
  assert.ok(emojiHandle, emoji.content[1].text);
  const readLocal = (args) =>
    client.callTool({ name: 'mcphub_call', arguments: { server: 'mcphub', tool: 'read_result', arguments: args } });
  const oneChar = await readLocal({ handle: emojiHandle, offset: 999, length: 1 });
  assert.equal(text(oneChar), '😀');
  assert.match(oneChar.content[1].text, /Next offset: 1001\./);
  const inside = await readLocal({ handle: emojiHandle, offset: 1000 });
  assert.equal(inside.isError, true);
  assert.match(text(inside), /inside a character/);

  // A long text is cut and the rest is read by handle, to the last character.
  const cut = await client.callTool(call('big_text', { max_chars: 1000 }));
  assert.equal(text(cut), 'a'.repeat(1000));
  const handle = /"handle":"(r\d+)","offset":1000/.exec(cut.content[1].text)?.[1];
  assert.ok(handle, cut.content[1].text);
  const readResult = (args) =>
    client.callTool({ name: 'mcphub_call', arguments: { server: 'mcphub', tool: 'read_result', arguments: args } });
  const middle = await readResult({ handle, offset: 1000, length: 1000 });
  assert.equal(text(middle), `${'a'.repeat(500)}${'b'.repeat(500)}`);
  assert.match(middle.content[1].text, /Next offset: 2000\./);
  const tail = await readResult({ handle, offset: 2000 });
  assert.equal(text(tail), 'b'.repeat(1000));
  assert.match(tail.content[1].text, /End of result\./);
  assert.equal((await readResult({ handle: 'r999' })).isError, true);
  assert.equal(received.filter((item) => item.tool === 'read_result').length, 0, 'read_result stays local');

  // In a batch a JSON result is embedded as JSON, not as an escaped string.
  const shapedBatch = await client.callTool({
    name: 'mcphub_batch',
    arguments: {
      calls: [
        { id: 'rows', server: 'db', tool: 'list_rows', arguments: {}, max_rows: 1 },
        { id: 'text', server: 'db', tool: 'read_row', arguments: {} }
      ]
    }
  });
  const shapedResults = JSON.parse(text(shapedBatch)).results;
  assert.deepEqual(shapedResults[0], {
    id: 'rows',
    ok: true,
    status: 'ok',
    result: { rows: [{ id: 1, name: 'alpha' }], tags: [] },
    notes: ['rows: kept 1 of 3 rows (max_rows).']
  });
  assert.equal(shapedResults[1].result, 'done read_row');

  // The stdio fallback (its own copy of the OAuth tokens) is left as soon as the daemon is back.
  daemonUp = false;
  const fallbackClient = new Client({ name: 'mcphub-offline-fallback', version: '1.0.0' });
  await fallbackClient.connect(
    new StdioClientTransport({
      command: process.execPath,
      args: [process.env.MCPHUB_CLIENT_ENTRY ?? 'index.js'],
      cwd: new URL('.', import.meta.url).pathname,
      env: {
        ...process.env,
        MCPHUB_HOME: home,
        MCPHUB_DAEMON_URL: `http://127.0.0.1:${port}`,
        MCPHUB_BINARY: new URL('test-fixtures/fake-stdio-hub.js', import.meta.url).pathname,
        MCPHUB_DAEMON_RETRY_MS: '100'
      },
      stderr: 'pipe'
    })
  );
  try {
    const read = { name: 'mcphub_call', arguments: { server: 'db', tool: 'read_row', arguments: { id: 2 } } };
    assert.equal(text(await fallbackClient.callTool(read)), 'stdio read_row');
    daemonUp = true;
    await new Promise((resolve) => setTimeout(resolve, 150));
    assert.equal(text(await fallbackClient.callTool(read)), 'done read_row');
  } finally {
    await fallbackClient.close();
  }

  // For Claude Code, a fresh trace from the MemoryPilot SessionStart hook means
  // the memory is already in context: bootstrap only asks for the episodes
  // matching the hints. HOME is the test folder, so a trace for ~ can be tested.
  const workspace = join(home, 'workspace');
  const traces = join(home, 'memorypilot', 'session-hook-injections.json');
  const writeTrace = (ageMs, root = workspace) =>
    writeFile(traces, JSON.stringify({ [root]: new Date(Date.now() - ageMs).toISOString() }));
  const recalls = () => [
    received.filter((call) => call.tool === 'recall' && !call.arguments?.episodes_only).length,
    received.filter((call) => call.tool === 'recall' && call.arguments?.episodes_only).length
  ];
  const bootstrap = (hints) => ({
    name: 'mcphub_bootstrap',
    arguments: { working_dir: workspace, ...(hints ? { hints } : {}) }
  });

  const claudeCode = new Client({ name: 'claude-code', version: '2.0.0' });
  await claudeCode.connect(
    new StdioClientTransport({
      command: process.execPath,
      args: [process.env.MCPHUB_CLIENT_ENTRY ?? 'index.js'],
      cwd: new URL('.', import.meta.url).pathname,
      env: { ...process.env, ...clientEnv, HOME: home },
      stderr: 'pipe'
    })
  );
  try {
    await claudeCode.callTool(bootstrap('deploy'));
    assert.deepEqual(recalls(), [1, 0], 'no trace: full recall');

    await writeTrace(60_000);
    const fresh = JSON.parse(text(await claudeCode.callTool(bootstrap('deploy'))));
    assert.deepEqual(recalls(), [1, 1], 'fresh trace: episodes only');
    assert.match(fresh.memory, /SessionStart hook/);
    assert.deepEqual(fresh.episodes, [{ id: 'ep-1' }]);

    await claudeCode.callTool(bootstrap());
    assert.deepEqual(recalls(), [1, 1], 'fresh trace, no hints: no call');

    // Another client with the same fresh trace still recalls everything.
    await client.callTool(bootstrap('deploy'));
    assert.deepEqual(recalls(), [2, 1], 'not Claude Code: full recall');

    await writeTrace(11 * 60_000);
    await claudeCode.callTool(bootstrap('deploy'));
    assert.deepEqual(recalls(), [3, 1], 'stale trace: full recall');

    // A session opened in ~ did not get the memory of the projects inside it.
    await writeTrace(60_000, home);
    await claudeCode.callTool(bootstrap('deploy'));
    assert.deepEqual(recalls(), [4, 1], 'trace for ~: full recall');

    // Claude Code: names against names.
    const savings = JSON.parse(text(await claudeCode.callTool({ name: 'mcphub_savings', arguments: {} })));
    assert.equal(savings.client, 'claude-code');
    const versus = savings.versusClaudeCodeDeferredLoading;
    assert.equal(versus.directCatalogBytes, Buffer.byteLength('mcp__db__read_row\n'));
    const hubNames = (await claudeCode.listTools()).tools.map((tool) => `mcp__McpHub__${tool.name}\n`);
    assert.equal(hubNames.length, 6);
    assert.equal(versus.hubCatalogBytes, Buffer.byteLength(hubNames.join('')));
  } finally {
    await claudeCode.close();
  }

  console.log('offline client tests: ok');
} finally {
  await client.close();
  http.closeAllConnections();
  http.close();
  await rm(home, { recursive: true, force: true });
}
