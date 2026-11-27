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
const runs = [];
// A daemon that predates script policies does not declare `policy` on mcphub_run.
let declareRunPolicy = true;
// A daemon that predates tenants does not declare `_mcphub_context` on mcphub_call.
let declareTenantContext = true;

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
        confirmation_token: z.string().optional(),
        ...(declareTenantContext ? { _mcphub_context: z.record(z.string(), z.unknown()).optional() } : {})
      }
    },
    async (args) => {
      received.push(args);
      if (args.tool === 'send_email') return new Promise(() => {});
      if (args.tool === 'slow_report') {
        await new Promise((resolve) => setTimeout(resolve, 600));
        return { content: [{ type: 'text', text: 'report ready' }] };
      }
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
  hub.registerTool(
    'mcphub_run',
    {
      inputSchema: {
        code: z.string(),
        timeout_secs: z.number().optional(),
        ...(declareRunPolicy ? { policy: z.record(z.string(), z.unknown()).optional() } : {})
      }
    },
    async (args) => {
      runs.push(args);
      const payload = { calls: 2, duration_ms: 5, logs: [], result: 42, ...(args.policy ? { policy: 'enforced' } : {}) };
      return { content: [{ type: 'text', text: JSON.stringify(payload) }] };
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
const acme = join(home, 'acme');
await writeFile(
  join(home, 'projects.json'),
  JSON.stringify({
    projects: {
      acme: {
        workspaces: [acme],
        aliases: { supabaseProjectId: 'ref-acme' },
        allowedServers: ['db', 'supabase'],
        ownedServers: ['acme-crm']
      },
      open: {
        workspaces: [join(home, 'open')],
        aliases: { supabaseProjectId: 'ref-open' },
        allowedServers: ['*']
      },
      beta: {
        workspaces: [join(home, 'beta')],
        aliases: {},
        tenant: 'beta',
        // allowedServers "*" never widens a tenant's perimeter.
        allowedServers: ['*'],
        credentials: { supabase: { SUPABASE_ACCESS_TOKEN: 'vault:beta/SUPABASE_ACCESS_TOKEN' } },
        sharedServers: ['context7']
      }
    }
  })
);
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

  // Terminal noise goes: colour codes, a progress bar redrawn in place, the same line many times.
  const noisy = [
    '\u001b[32mPASS\u001b[0m suite one',
    'downloading 10%\rdownloading 55%\rdownloading 100%',
    ...Array.from({ length: 7 }, () => 'warning: deprecated call'),
    'done',
  ].join('\n');
  const quiet = await fixed(noisy);
  assert.equal(text(quiet), 'PASS suite one\ndownloading 100%\nwarning: deprecated call\n[McpHub: the line above came 7 times in a row]\ndone');
  assert.match(quiet.content[1].text, /Terminal noise removed/);
  assert.equal(text(await fixed(noisy, { raw: true })), noisy);
  // An ordinary text, a source file with a few equal lines, Windows line endings: untouched.
  for (const plain of ['one\ntwo\n\n\nthree', 'if (a) {\n}\n}\n}\n}\nend', 'line one\r\nline two\r\n', 'a,b\n1,2']) {
    const same = await fixed(plain);
    assert.equal(text(same), plain);
    assert.equal(same.content.length, 1, plain);
  }

  // Short lines repeated in a row are data, not noise; an odd line ending loses nothing.
  for (const data of ['0\n0\n0\n0\n0\n0', 'a\n}\n}\n}\n}\n}\n}', '---\n---\n---\n---\n---', 'x\r\r\ny']) {
    const same = await fixed(data);
    assert.equal(text(same), data);
    assert.equal(same.content.length, 1, JSON.stringify(data));
  }
  // JSON that McpHub does not rewrite (a 16-digit number) is not cleaned either, and says why a format was skipped.
  const bigNumber = `[\n{"id":12345678901234567},\n${Array.from({ length: 6 }, () => '{"status":"pending","count":0},').join('\n')}\n{"id":2}\n]`;
  assert.equal(text(await fixed(bigNumber)), bigNumber);
  const skipped = await fixed(bigNumber, { format: 'csv' });
  assert.equal(text(skipped), bigNumber);
  assert.match(skipped.content[1].text, /format csv not applied/);

  // csv: one header line and one line per row, cells quoted only when needed.
  const people = [{ id: 1, name: 'Ann, Lee', tags: ['a', 'b'] }, { id: 2, name: 'Bob "B"', city: 'Paris' }];
  assert.equal(text(await fixed(JSON.stringify(people), { format: 'csv' })), 'id,name,tags,city\n1,"Ann, Lee","[""a"",""b""]",\n2,"Bob ""B""",,Paris');
  assert.equal(
    text(await fixed(JSON.stringify({ total: 2, rows: [{ id: 1 }, { id: 2 }] }), { format: 'csv', fields: ['id'] })),
    '# total: 2\n# rows:\nid\n1\n2'
  );
  assert.equal(
    text(await fixed(JSON.stringify({ note: 'two\nlines', rows: [{ id: 1 }] }), { format: 'csv' })),
    '# note: "two\\nlines"\n# rows:\nid\n1'
  );
  assert.match((await fixed('[{},{}]', { format: 'csv' })).content[1].text, /csv needs a list of rows/);
  const notRows = await fixed('{"a":{"b":1}}', { format: 'csv' });
  assert.equal(text(notRows), '{"a":{"b":1}}');
  assert.match(notRows.content[1].text, /csv needs a list of rows/);

  // A cut result can be searched instead of read through: the passages that hold the words, with their place.
  const haystack = `${'filler '.repeat(300)}first needle here ${'filler '.repeat(300)}the needle and the thread together ${'filler '.repeat(300)}`;
  const cutHay = await fixed(haystack, { max_chars: 1000 });
  const hayHandle = /"handle":"(r\d+)","query"/.exec(cutHay.content[1].text)?.[1];
  assert.ok(hayHandle, cutHay.content[1].text);
  const searchLocal = (args) =>
    client.callTool({ name: 'mcphub_call', arguments: { server: 'mcphub', tool: 'search_result', arguments: args } });
  const found = await searchLocal({ handle: hayHandle, query: 'Needle thread', context: 40 });
  assert.equal(found.content.length, 3);
  assert.match(found.content[0].text, /^\[chars \d+ to \d+\]\n.*first needle here/s);
  assert.match(found.content[1].text, /the needle and the thread together/);
  assert.match(found.content[2].text, /2 of 2 passages shown, 3 matches in \d+ chars\./);
  assert.ok(found.content.map((item) => item.text).join('').length < 600, 'a search answers in a few hundred chars');
  // The offset it gives is the one read_result takes.
  const at = Number(/^\[chars (\d+) to/.exec(found.content[1].text)[1]);
  assert.equal(text(await readLocal({ handle: hayHandle, offset: at, length: 30 })), haystack.slice(at, at + 30));
  // The passage holding both words comes first when only one is asked for.
  const best = await searchLocal({ handle: hayHandle, query: 'needle thread', context: 40, max_matches: 1 });
  assert.match(best.content[0].text, /thread together/);
  const none = await searchLocal({ handle: hayHandle, query: 'absent', context: 40 });
  assert.match(text(none), /No match for "absent"/);
  const partly = await searchLocal({ handle: hayHandle, query: 'needle absent', context: 40 });
  assert.match(partly.content.at(-1).text, /Never found: "absent"\./);
  // Letters whose lower case is longer, and characters made of two units: the places given stay right.
  const tricky = `${'İ'.repeat(40)} ${'😀'.repeat(60)}needle ${'z'.repeat(1200)}`;
  const trickyCut = await fixed(tricky, { max_chars: 1000 });
  const trickyHandle = /"handle":"(r\d+)","query"/.exec(trickyCut.content[1].text)?.[1];
  const inTricky = await searchLocal({ handle: trickyHandle, query: 'NEEDLE', context: 21 });
  const trickyFrom = Number(/^\[chars (\d+) to/.exec(inTricky.content[0].text)[1]);
  assert.match(inTricky.content[0].text, /needle/);
  const reread = await readLocal({ handle: trickyHandle, offset: trickyFrom, length: 60 });
  assert.notEqual(reread.isError, true, text(reread));
  assert.ok(text(reread).includes('needle'));
  // A very common word does not hide a rare one, and the answer stays small whatever is asked.
  const log = `${'the quick line\n'.repeat(9000)}one error at the end\n`;
  const logCut = await fixed(log, { max_chars: 1000 });
  const logHandle = /"handle":"(r\d+)","query"/.exec(logCut.content[1].text)?.[1];
  const rare = await searchLocal({ handle: logHandle, query: 'the error', context: 2000, max_matches: 50 });
  assert.doesNotMatch(rare.content.at(-1).text, /Never found/);
  assert.match(rare.content.at(-1).text, /5001\+ matches|\d+\+ matches/);
  assert.ok(rare.content.some((item) => item.text.includes('one error at the end')), 'the passage with both words is shown');
  assert.ok(rare.content.map((item) => item.text).join('').length < 40_000, 'bounded answer');
  assert.equal((await searchLocal({ handle: 'r999', query: 'x' })).isError, true);
  assert.equal((await searchLocal({ handle: hayHandle, query: '  ' })).isError, true);
  assert.equal((await searchLocal({ handle: hayHandle, query: 'x', context: 5 })).isError, true);

  // A large list answered without fields or max_rows reminds the agent of them; a shaped one does not.
  const manyRows = JSON.stringify({ rows: Array.from({ length: 200 }, (_, id) => ({ id, label: 'x'.repeat(40) })) });
  const hinted = await fixed(manyRows);
  assert.equal(text(hinted), manyRows);
  assert.match(hinted.content[1].text, /Large result: \d+ chars for 200 rows\. Next time pass fields/);
  assert.equal((await fixed(manyRows, { max_rows: 150 })).content[1].text, '[McpHub] rows: kept 150 of 200 rows (max_rows).');

  // The advice reaches every client at startup.
  assert.match(client.getInstructions() ?? '', /pass fields .* and max_rows/);

  // format toon writes the rows once under one header line.
  const toon = await client.callTool(call('list_rows', { format: 'toon', fields: ['id', 'name'] }));
  assert.equal(text(toon), 'rows[3]{id,name}:\n  1,alpha\n  2,beta\n  3,gamma\ntags: []');

  // A script outside any project runs without a policy, except for servers owned by a project.
  const run = (extra = {}) => client.callTool({ name: 'mcphub_run', arguments: { code: 'return 42;', ...extra } });
  assert.equal(JSON.parse(text(await run())).result, 42);
  assert.deepEqual(runs.at(-1).policy, { denied_servers: ['acme-crm'] });

  // Each run counts once, with the tool calls its script made: the round trips it saved.
  const savingsReport = JSON.parse(text(await client.callTool({ name: 'mcphub_savings', arguments: {} })));
  const codeMode = savingsReport.codeMode;

  // Each server is listed with its calls and the bytes it sent to the agent, heaviest first.
  const perServer = savingsReport.perServer.servers;
  assert.ok(perServer.length > 0);
  for (const entry of perServer) {
    assert.ok(entry.calls > 0 && entry.resultBytes > 0, JSON.stringify(entry));
    assert.equal(entry.approximateTokens, Math.round(entry.resultBytes / 4));
    // Size returned by the child server before shaping, and what shaping removed from it.
    assert.ok(entry.rawBytes > 0 && entry.savedBytes >= 0 && entry.savedBytes < entry.rawBytes, JSON.stringify(entry));
    assert.equal(entry.approximateTokensSaved, Math.round(entry.savedBytes / 4));
  }
  const shaping = savingsReport.perServer.shaping;
  assert.equal(shaping.rawBytes, perServer.reduce((sum, entry) => sum + entry.rawBytes, 0));
  assert.equal(shaping.savedBytes, perServer.reduce((sum, entry) => sum + entry.savedBytes, 0));
  assert.ok(shaping.savedPercent >= 0 && shaping.savedPercent <= 100, JSON.stringify(shaping));
  assert.deepEqual(perServer.map((entry) => entry.resultBytes), perServer.map((entry) => entry.resultBytes).sort((a, b) => b - a));
  // A call refused before any result came back (bad shaping arguments) is counted in the total only.
  const deliveredCalls = perServer.reduce((sum, entry) => sum + entry.calls, 0);
  assert.ok(deliveredCalls > 0 && deliveredCalls <= savingsReport.actualUsage.calls);
  assert.equal(savingsReport.actualUsage.byServer, undefined);
  assert.equal(codeMode.runs, 1);
  assert.equal(codeMode.callsInsideScripts, 2);
  assert.equal(codeMode.roundTripsSaved, 1);

  // Inside a project the daemon receives the same rules mcphub_call enforces.
  assert.equal(JSON.parse(text(await run({ working_dir: acme }))).policy, 'enforced');
  assert.deepEqual(runs.at(-1).policy, {
    allowed_servers: ['db', 'supabase', 'acme-crm'],
    pinned_args: { supabase: { project_id: 'ref-acme' } }
  });

  // allowedServers ["*"] opens every server and keeps the pinned identifiers.
  const open = join(home, 'open');
  const anyServer = await client.callTool({
    name: 'mcphub_call',
    arguments: { server: 'mail', tool: 'read_row', arguments: {}, working_dir: open }
  });
  assert.equal(text(anyServer), 'done read_row');
  const wrongProject = await client.callTool({
    name: 'mcphub_call',
    arguments: { server: 'supabase', tool: 'read_row', arguments: { project_id: 'other' }, working_dir: open }
  });
  assert.equal(wrongProject.isError, true);
  assert.match(text(wrongProject), /Supabase project mismatch for open/);
  await run({ working_dir: open });
  assert.deepEqual(runs.at(-1).policy, {
    denied_servers: ['acme-crm'],
    pinned_args: { supabase: { project_id: 'ref-open' } }
  });

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

  // Tenant: the caller's folders reach the daemon, which decides; the client only
  // lets the tenant's own servers and the shared ones through.
  const beta = join(home, 'beta');
  const tenantCall = await client.callTool({
    name: 'mcphub_call',
    arguments: { server: 'supabase', tool: 'list_tables', arguments: {}, working_dir: join(beta, 'site') }
  });
  assert.equal(text(tenantCall), 'done list_tables');
  const sent = received.findLast((call) => call.tool === 'list_tables');
  assert.equal(sent._mcphub_context.working_dir, join(beta, 'site'));
  assert.ok(sent._mcphub_context.cwd, 'the client folder goes along');
  const outside = await client.callTool({
    name: 'mcphub_call',
    arguments: { server: 'github', tool: 'list_issues', arguments: {}, working_dir: beta }
  });
  assert.equal(outside.isError, true);
  assert.match(text(outside), /not allowed for project "beta"/);
  assert.equal(received.filter((call) => call.tool === 'list_issues').length, 0);
  const shared = await client.callTool({
    name: 'mcphub_call',
    arguments: { server: 'context7', tool: 'resolve', arguments: {}, working_dir: beta }
  });
  assert.equal(text(shared), 'done resolve');

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

  declareRunPolicy = false;
  declareTenantContext = false;
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
    assert.equal(hubNames.length, 7);

    // A daemon too old to enforce project rules in a script never receives one.
    const runsBefore = runs.length;
    const refused = await claudeCode.callTool({
      name: 'mcphub_run',
      arguments: { code: 'return 1;', working_dir: acme }
    });
    assert.equal(refused.isError, true);
    assert.match(text(refused), /too old to enforce/);
    assert.equal(runs.length, runsBefore);
    assert.equal(versus.hubCatalogBytes, Buffer.byteLength(hubNames.join('')));

    // A daemon too old to isolate tenants never gets a call from a tenant folder.
    const tablesBefore = received.filter((call) => call.tool === 'list_tables').length;
    const tooOld = await claudeCode.callTool({
      name: 'mcphub_call',
      arguments: { server: 'supabase', tool: 'list_tables', arguments: {}, working_dir: beta }
    });
    assert.equal(tooOld.isError, true);
    assert.match(text(tooOld), /too old to isolate tenant accounts/);
    assert.equal(received.filter((call) => call.tool === 'list_tables').length, tablesBefore);
    // Outside tenant folders, the same daemon keeps working.
    const plain = await claudeCode.callTool({
      name: 'mcphub_call',
      arguments: { server: 'db', tool: 'read_row', arguments: { id: 9 }, working_dir: acme }
    });
    assert.equal(text(plain), 'done read_row');
  } finally {
    await claudeCode.close();
  }

  // Several windows share one statistics file: each adds what it counted, none erases the others.
  const windows = await Promise.all(
    [1, 2, 3].map(async () => {
      const window = new Client({ name: 'mcphub-offline-window', version: '1.0.0' });
      await window.connect(
        new StdioClientTransport({
          command: process.execPath,
          args: [process.env.MCPHUB_CLIENT_ENTRY ?? 'index.js'],
          cwd: new URL('.', import.meta.url).pathname,
          env: { ...process.env, ...clientEnv },
          stderr: 'pipe'
        })
      );
      return window;
    })
  );
  try {
    const windowCall = { name: 'mcphub_call', arguments: { server: 'windows', tool: 'read_row', arguments: { id: 1 } } };
    await Promise.all(windows.flatMap((window) => [1, 2, 3, 4].map(() => window.callTool(windowCall))));
    // Every window writes at the same moment, then any of them reports the total of all.
    const reports = await Promise.all(
      windows.map(async (window) => JSON.parse(text(await window.callTool({ name: 'mcphub_savings', arguments: {} }))))
    );
    const lastReport = JSON.parse(text(await windows[0].callTool({ name: 'mcphub_savings', arguments: {} })));
    const shared = lastReport.perServer.servers.find((entry) => entry.server === 'windows');
    assert.equal(shared.calls, 12);
    // The same answer twelve times: the bytes add up exactly.
    assert.ok(shared.resultBytes > 0 && shared.resultBytes % 12 === 0, String(shared.resultBytes));
    assert.ok(reports.every((report) => report.perServer.servers.some((entry) => entry.server === 'windows')));
  } finally {
    await Promise.all(windows.map((window) => window.close()));
  }

  // A call longer than the answer delay gets a waiting answer instead of being cut by the
  // MCP client; it keeps running once, and its result is read by handle.
  const patient = new Client({ name: 'mcphub-offline-pending', version: '1.0.0' });
  await patient.connect(
    new StdioClientTransport({
      command: process.execPath,
      args: [process.env.MCPHUB_CLIENT_ENTRY ?? 'index.js'],
      cwd: new URL('.', import.meta.url).pathname,
      env: {
        ...process.env,
        ...clientEnv,
        MCPHUB_ANSWER_BEFORE_MS: '200',
        MCPHUB_CALL_TIMEOUT_MS: '900'
      },
      stderr: 'pipe'
    })
  );
  try {
    const callResult = (handle) =>
      patient.callTool({
        name: 'mcphub_call',
        arguments: { server: 'mcphub', tool: 'call_result', arguments: { handle } }
      });
    const slow = await patient.callTool({
      name: 'mcphub_call',
      arguments: { server: 'reports', tool: 'slow_report', arguments: {} }
    });
    assert.notEqual(slow.isError, true);
    assert.equal(slow._meta?.mcphub?.status, 'pending');
    assert.equal(slow._meta.mcphub.handle, 'c1');
    assert.match(text(slow), /Do not call it again.*"handle":"c1"/);

    // Still running: the same handle is given again, nothing is replayed.
    const waiting = await callResult('c1');
    assert.equal(waiting._meta?.mcphub?.status, 'pending');
    assert.equal(waiting._meta.mcphub.handle, 'c1');

    let ready = await callResult('c1');
    while (ready._meta?.mcphub?.status === 'pending') ready = await callResult('c1');
    assert.equal(text(ready), 'report ready');
    assert.equal(received.filter((call) => call.tool === 'slow_report').length, 1);

    // A result is read once.
    const gone = await callResult('c1');
    assert.equal(gone.isError, true);
    assert.match(text(gone), /No pending call for handle "c1"/);

    // A quick call is answered directly.
    const quick = await patient.callTool({
      name: 'mcphub_call',
      arguments: { server: 'db', tool: 'read_row', arguments: { id: 3 } }
    });
    assert.equal(text(quick), 'done read_row');
    assert.equal(quick._meta?.mcphub?.status, undefined);

    // A call that fails in the background reports its failure through the handle.
    const stuck = await patient.callTool({
      name: 'mcphub_call',
      arguments: { server: 'mail', tool: 'send_email', arguments: { to: 'b@example.com' } }
    });
    assert.equal(stuck._meta?.mcphub?.status, 'pending');
    let failed = await callResult(stuck._meta.mcphub.handle);
    while (failed._meta?.mcphub?.status === 'pending') failed = await callResult(stuck._meta.mcphub.handle);
    assert.equal(failed.isError, true);
    assert.equal(failed._meta?.mcphub?.status, 'unknown_outcome');
  } finally {
    await patient.close();
  }

  console.log('offline client tests: ok');
} finally {
  await client.close();
  http.closeAllConnections();
  http.close();
  await rm(home, { recursive: true, force: true });
}
