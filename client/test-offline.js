// Offline tests: index.js against a fake McpHub daemon (SSE), no real servers,
// no user files. Run with `npm run test:offline`.
import assert from 'node:assert/strict';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
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
      if (args.tool === 'delete_row' && !args.confirmation_token) {
        return {
          content: [{ type: 'text', text: 'CONFIRMATION REQUISE' }],
          isError: false,
          _meta: { mcphub: { status: 'confirmation_required', confirmation_token: 'tok-1' } }
        };
      }
      return { content: [{ type: 'text', text: `done ${args.tool}` }] };
    }
  );
  return hub;
}

const transports = new Map();
const http = createServer(async (req, res) => {
  if (req.headers.authorization !== `Bearer ${token}`) {
    res.writeHead(401).end();
    return;
  }
  const url = new URL(req.url, 'http://127.0.0.1');
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
await writeFile(join(home, 'projects.json'), '{"projects":{}}');

const client = new Client({ name: 'mcphub-offline-test', version: '1.0.0' });
await client.connect(
  new StdioClientTransport({
    command: process.execPath,
    args: ['index.js'],
    cwd: new URL('.', import.meta.url).pathname,
    env: {
      ...process.env,
      MCPHUB_HOME: home,
      MCPHUB_DAEMON_URL: `http://127.0.0.1:${port}`,
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

  console.log('offline client tests: ok');
} finally {
  await client.close();
  http.closeAllConnections();
  http.close();
  await rm(home, { recursive: true, force: true });
}
