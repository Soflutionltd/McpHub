#!/usr/bin/env node
// Stand-in for `McpHub stdio` in the offline tests: answers every call with "stdio <tool>".
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { z } from 'zod';

const hub = new McpServer({ name: 'fake-stdio-hub', version: '1.0.0' });
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
  async (args) => ({ content: [{ type: 'text', text: `stdio ${args.tool}` }] })
);
await hub.connect(new StdioServerTransport());
