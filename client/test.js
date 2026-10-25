// Smoke tests against the local McpHub daemon. Prerequisites: `McpHub serve`
// running, with the MemoryPilot and supabase servers configured. The project
// policy under test is a throwaway file, never the user's own projects.json.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';

const sandbox = await mkdtemp(join(tmpdir(), 'mcphub-client-test-'));
const demoWorkspace = join(sandbox, 'demo');
const unmappedWorkspace = join(sandbox, 'unmapped');
const acmeWorkspace = join(sandbox, 'acme');
const projectsPath = join(sandbox, 'projects.json');
await writeFile(
  projectsPath,
  JSON.stringify({
    projects: {
      demo: {
        workspaces: [demoWorkspace],
        aliases: {
          product: 'Demo',
          supabaseProjectId: 'demo-project-ref',
          appStoreAppId: '1234567890',
          iosBundleId: 'com.example.demo',
          cloudflareProject: 'demo-web'
        },
        allowedServers: ['MemoryPilot', 'supabase']
      },
      // A client project: context7 stands in for the client's own Stripe.
      acme: {
        workspaces: [acmeWorkspace],
        aliases: { product: 'Acme' },
        allowedServers: ['MemoryPilot'],
        ownedServers: ['context7']
      }
    }
  })
);

const client = new Client({
  name: 'mcphub-client-test',
  version: '1.0.0'
});
const transport = new StdioClientTransport({
  command: process.execPath,
  args: ['index.js'],
  cwd: new URL('.', import.meta.url).pathname,
  env: { ...process.env, MCPHUB_PROJECTS: projectsPath },
  stderr: 'pipe'
});

function text(result) {
  return result.content[0].text;
}

try {
  await client.connect(transport);
  const { tools } = await client.listTools();
  const names = new Set(tools.map((tool) => tool.name));

  for (const expected of [
    'mcphub_bootstrap',
    'mcphub_search',
    'mcphub_call',
    'mcphub_batch',
    'mcphub_status',
    'mcphub_savings'
  ]) {
    assert(names.has(expected), `Missing tool: ${expected}`);
  }
  const exposedBytes = Buffer.byteLength(JSON.stringify(tools));
  process.stdout.write(`Exposed catalog: ${tools.length} tools, ${exposedBytes} bytes\n`);

  // Un appel direct ne doit pas déclencher MemoryPilot : on mesure le temps.
  const startedAt = performance.now();
  const status = await client.callTool({
    name: 'mcphub_status',
    arguments: { server: 'MemoryPilot' }
  });
  assert.equal(status.isError, undefined);
  assert.match(text(status), /MemoryPilot/);
  process.stdout.write(`mcphub_status: ${Math.round(performance.now() - startedAt)} ms\n`);

  const search = await client.callTool({
    name: 'mcphub_search',
    arguments: {
      working_dir: demoWorkspace,
      query: 'list Supabase projects',
      top_k: 3
    }
  });
  assert.equal(search.isError, undefined);
  assert.match(text(search), /"policy":"enforced"/);

  const guarded = await client.callTool({
    name: 'mcphub_call',
    arguments: {
      working_dir: demoWorkspace,
      server: 'supabase',
      tool: 'get_project',
      arguments: { id: 'wrong-project', project_id: 'wrong-project' }
    }
  });
  assert.equal(guarded.isError, true);
  assert.match(text(guarded), /project mismatch/);

  const unknownTool = await client.callTool({
    name: 'mcphub_call',
    arguments: {
      working_dir: demoWorkspace,
      server: 'MemoryPilot',
      tool: 'recal',
      arguments: {}
    }
  });
  assert.equal(unknownTool.isError, true);
  const unknownPayload = JSON.parse(text(unknownTool));
  assert.match(unknownPayload.error, /Unknown tool/);
  assert.equal(
    unknownPayload.suggestions[0]?.tool,
    'recall',
    'Expected "recall" as first suggestion for "recal".'
  );

  const unknownServer = await client.callTool({
    name: 'mcphub_call',
    arguments: {
      server: 'supabaze',
      tool: 'list_tables',
      arguments: {}
    }
  });
  assert.equal(unknownServer.isError, true);
  const unknownServerPayload = JSON.parse(text(unknownServer));
  assert.equal(unknownServerPayload.similarServers?.[0], 'supabase');
  assert.equal(unknownServerPayload.suggestions?.[0]?.server, 'supabase');

  const batch = await client.callTool({
    name: 'mcphub_batch',
    arguments: {
      working_dir: demoWorkspace,
      calls: [
        {
          id: 'memory',
          server: 'MemoryPilot',
          tool: 'list_projects',
          arguments: {}
        },
        {
          id: 'outside-policy',
          server: 'appstore-operator',
          tool: 'health_check',
          arguments: {}
        }
      ]
    }
  });
  assert.equal(batch.isError, undefined);
  assert.match(text(batch), /"ok":true/);
  assert.match(text(batch), /not allowed for project/);

  const bootstrap = await client.callTool({
    name: 'mcphub_bootstrap',
    arguments: {
      working_dir: demoWorkspace,
      hints: 'McpHub smoke test'
    }
  });
  assert.equal(bootstrap.isError, undefined);
  assert.match(text(bootstrap), /demo-project-ref/);

  // A folder without a mapping still loads the memory, with no policy.
  const unmapped = await client.callTool({
    name: 'mcphub_bootstrap',
    arguments: {
      working_dir: unmappedWorkspace,
      hints: 'McpHub smoke test'
    }
  });
  assert.equal(unmapped.isError, undefined);
  const unmappedPayload = JSON.parse(text(unmapped));
  assert.equal(unmappedPayload.project, null);
  assert.match(unmappedPayload.policy, /^none/);
  assert.equal(unmappedPayload.allowedServers, undefined);
  assert.equal(typeof unmappedPayload.memory, 'string', unmappedPayload.memoryError);

  // A sibling folder sharing the prefix is not inside the project.
  const sibling = await client.callTool({
    name: 'mcphub_bootstrap',
    arguments: { working_dir: `${demoWorkspace}2`, hints: 'McpHub smoke test' }
  });
  assert.equal(JSON.parse(text(sibling)).project, null);

  // A client-owned server only answers from that client's folder.
  const ownedCall = (workingDir) =>
    client.callTool({
      name: 'mcphub_call',
      arguments: {
        ...(workingDir ? { working_dir: workingDir } : {}),
        server: 'context7',
        tool: 'no-such-tool',
        arguments: {}
      }
    });
  for (const outsider of [demoWorkspace, unmappedWorkspace, undefined]) {
    const refused = await ownedCall(outsider);
    assert.equal(refused.isError, true);
    assert.match(text(refused), /belongs to project \\?"acme\\?"/);
  }
  const fromAcme = JSON.parse(text(await ownedCall(join(acmeWorkspace, 'src'))));
  assert.match(fromAcme.error, /Unknown tool/);
  assert(fromAcme.suggestions.every((item) => item.server === 'context7'));

  const outsiderSearch = JSON.parse(
    text(
      await client.callTool({
        name: 'mcphub_search',
        arguments: { working_dir: unmappedWorkspace, query: 'library documentation', top_k: 20 }
      })
    )
  );
  assert(!outsiderSearch.results.some((item) => item.server === 'context7'));
  assert(!(outsiderSearch.available_servers ?? []).includes('context7'));

  const outsiderScopedSearch = await client.callTool({
    name: 'mcphub_search',
    arguments: { working_dir: demoWorkspace, server: 'context7', query: 'docs' }
  });
  assert.equal(outsiderScopedSearch.isError, true);

  const acmeSearch = JSON.parse(
    text(
      await client.callTool({
        name: 'mcphub_search',
        arguments: { working_dir: acmeWorkspace, server: 'context7', query: 'library documentation' }
      })
    )
  );
  assert(acmeSearch.results.some((item) => item.server === 'context7'));

  const statusFor = async (workingDir) =>
    JSON.parse(
      text(
        await client.callTool({
          name: 'mcphub_status',
          arguments: { server: 'context7', working_dir: workingDir }
        })
      )
    ).servers;
  assert.equal((await statusFor(unmappedWorkspace)).length, 0);
  assert.equal((await statusFor(acmeWorkspace)).length, 1);

  const acmeBatch = await client.callTool({
    name: 'mcphub_batch',
    arguments: {
      working_dir: demoWorkspace,
      calls: [{ id: 'foreign', server: 'context7', tool: 'resolve-library-id', arguments: {} }]
    }
  });
  assert.match(text(acmeBatch), /belongs to project/);

  const noFolder = await client.callTool({ name: 'mcphub_bootstrap', arguments: {} });
  assert.equal(noFolder.isError, true);

  const savings = await client.callTool({
    name: 'mcphub_savings',
    arguments: {}
  });
  assert.equal(savings.isError, undefined);
  const savingsPayload = JSON.parse(text(savings));
  assert.equal(typeof savingsPayload.versusCursorDeferredLoading.savedPercent, 'number');
  process.stdout.write(`Transport: ${savingsPayload.process.transport}\n`);

  // Two owners for one server is a configuration error, raised at start-up.
  const conflictPath = join(sandbox, 'conflict.json');
  await writeFile(
    conflictPath,
    JSON.stringify({
      projects: {
        one: { workspaces: [join(sandbox, 'one')], aliases: {}, ownedServers: ['stripe-client'] },
        two: { workspaces: [join(sandbox, 'two')], aliases: {}, ownedServers: ['stripe-client'] }
      }
    })
  );
  await assert.rejects(
    promisify(execFile)(process.execPath, ['index.js'], {
      cwd: new URL('.', import.meta.url).pathname,
      env: { ...process.env, MCPHUB_PROJECTS: conflictPath },
      timeout: 10_000
    }),
    /owned by both "one" and "two"/
  );

  // A named instance without its own port is refused before anything starts.
  await assert.rejects(
    promisify(execFile)(process.execPath, ['index.js'], {
      cwd: new URL('.', import.meta.url).pathname,
      env: { ...process.env, MCPHUB_INSTANCE: 'acme', MCPHUB_PORT: '' },
      timeout: 10_000
    }),
    /MCPHUB_PORT is required/
  );

  process.stdout.write('McpHub client smoke tests passed.\n');
} finally {
  await client.close();
  await rm(sandbox, { recursive: true, force: true });
}
