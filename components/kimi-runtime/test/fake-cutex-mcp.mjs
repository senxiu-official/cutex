// A minimal stdio MCP server named like cutex-mcp: one tool `agent_list` that
// answers with the thread id cutex gave it (proves the env reached it).
import { createInterface } from 'node:readline';
const send = message => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...message }) + '\n');
for await (const line of createInterface({ input: process.stdin })) {
  let message;
  try { message = JSON.parse(line); } catch { continue; }
  if (message.id === undefined) continue;
  if (message.method === 'initialize') send({ id: message.id, result: { protocolVersion: message.params?.protocolVersion ?? '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'cutex', version: '0' } } });
  else if (message.method === 'tools/list') send({ id: message.id, result: { tools: [{ name: 'agent_list', description: 'List agents', inputSchema: { type: 'object', properties: {} } }] } });
  else if (message.method === 'tools/call') send({ id: message.id, result: { content: [{ type: 'text', text: `agents: none; thread=${process.env.CUTEX_MCP_THREAD_ID}` }] } });
  else send({ id: message.id, error: { code: -32601, message: 'unsupported' } });
}
