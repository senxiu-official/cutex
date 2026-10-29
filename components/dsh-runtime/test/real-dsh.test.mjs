import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { createServer } from 'node:http';
import { DshRuntime, cutexMcp } from '../src/runtime.mjs';

// Explicit paths only: this test never discovers credentials or live deployments.
const install = process.env.DSH_SPIKE_INSTALL;
const mcpBinary = process.env.DSH_SPIKE_MCP;
test('real dsh --profile acp + real cutex-mcp: approve, call, close, restart, resume, replay, cancel', {
  skip: !install || !mcpBinary,
  timeout: 300000,
}, async t => {
  const base = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(base, { recursive: true });
  const home = mkdtempSync(join(base, 'real-'));
  const cwd = join(home, 'workspace');
  mkdirSync(cwd);
  writeFileSync(join(home, '.cutex-test-private-home'), 'dsh ACP spike\n');
  const paths = [];
  const calls = [];
  const bus = createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    paths.push(req.url);
    assert.equal(req.headers.authorization, 'Bearer mock-spike-only');
    const body = JSON.parse(Buffer.concat(chunks).toString() || '{}');
    res.setHeader('Content-Type', 'application/json');
    if (req.url === '/api/mcp/v1/toolset') res.end(JSON.stringify({ ok: true, tools: ['agent_list'] }));
    else if (req.url === '/api/asks/v1/resolve-ids') res.end(JSON.stringify({ ok: true, arguments: body.arguments }));
    else if (req.url === '/api/asks/v1' && body.tool === 'agent_list') {
      assert.equal(req.headers['x-cutex-mcp-thread-id'], '018e0000-0000-7000-8000-0000a000037f');
      assert.equal(req.headers['x-cutex-mcp-generation'], '1');
      calls.push(body);
      res.end(JSON.stringify({ ok: true, agents: [], offlineMarker: 'REAL_CUTEX_MCP_MOCK_BUS' }));
    } else { res.statusCode = 404; res.end(JSON.stringify({ ok: false })); }
  });
  await new Promise(resolve => bus.listen(0, '127.0.0.1', resolve));
  const port = bus.address().port;
  assert.ok(![24270, 24260, 25370, 25360].includes(port));
  const env = Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT'].filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'), TEMP: home, TMP: home, CUTEX_TEST_PRIVATE_HOME: home, DSH_HOME: join(home, 'dsh'), DSH_TELEMETRY_DISABLED: '1', DSH_SPIKE_INSTALL_ROOT: resolve(install) });
  const patch = join(home, 'offline.patch.yml');
  const plugin = fileURLToPath(new URL('./mock-llm.mjs', import.meta.url)).replaceAll('\\', '/');
  writeFileSync(patch, `
- id: llm-deepseek
  disabled: true
- id: acp
  config:
    provider: cutex-offline
    model: offline
- id: session-persistence-jsonl
  config:
    root: '${join(home, 'sessions').replaceAll('\\', '/')}'
    compression: none
- id: session-log-deepseek
  config:
    enabled: false
- insert:
    - id: cutex-spike-llm
      name: '${plugin}'
`);
  const mcp = cutexMcp(resolve(mcpBinary), { HOME: home, USERPROFILE: home, CUTEX_TEST_PRIVATE_HOME: home, CUTEX_AGENT_ID: 'dsh.spike', CUTEX_RUNTIME_GENERATION: '1', CUTEX_AGENT_BUS_URL: `http://127.0.0.1:${port}`, CUTEX_AGENT_BUS_TOKEN: 'mock-spike-only', CUTEX_MCP_THREAD_ID: '018e0000-0000-7000-8000-0000a000037f' });
  const approvals = [];
  let decision = 'accept';
  const config = { command: process.execPath, args: [join(install, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), '--profile', 'acp', '--patch', patch], cwd, env, database: join(home, 'timeline.sqlite'), sessionsRoot: join(home, 'sessions'), installRoot: resolve(install), approve: async request => { approvals.push(request); return { decision }; } };
  let runtime = new DshRuntime(config);
  t.after(async () => {
    if (runtime) await runtime.close();
    bus.closeAllConnections();
    await new Promise(resolve => bus.close(resolve));
    rmSync(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  });
  try {
    await runtime.initialize([mcp]);
    assert.equal((await runtime.prompt('query the offline agent list')).stopReason, 'end_turn');
    assert.ok(calls.length > 0, `no real MCP call; events=${JSON.stringify(runtime.replay())}`);
    // The fixture makes DSH ask for the cutex tool; cutex MCP tools are pre-approved (as for cute-codex and Claude).
    assert.equal(approvals.length, 0, 'a cutex MCP tool never reaches the owner');
    assert.ok(runtime.replay().some(e => e.method === 'item/completed' && e.params.item.type === 'agentMessage' && e.params.item.text.includes('REAL_CUTEX_MCP_MOCK_BUS')));
    const before = runtime.replay();
    const sessionId = runtime.sessionId;
    await runtime.close();
    runtime = new DshRuntime(config);
    await runtime.initialize([mcp]);
    assert.equal(runtime.sessionId, sessionId);
    assert.deepEqual(runtime.replay(), before, 'resume must retain own projection without duplicating it');
    assert.equal((await runtime.prompt('query again after restart')).stopReason, 'end_turn');
    assert.equal(calls.length, 2);
    decision = 'decline';
    await runtime.prompt('query the offline list; an owner decline does not apply to cutex tools');
    assert.equal(calls.length, 3, 'pre-approved cutex tool reaches the Bus');
    assert.equal(approvals.length, 0);
    const beforeCrash = runtime.replay();
    runtime.rpc.child.kill('SIGKILL');
    await runtime.rpc.exited;
    await runtime.close();
    runtime = new DshRuntime(config);
    await runtime.initialize([mcp]);
    assert.equal(runtime.sessionId, sessionId);
    assert.deepEqual(runtime.replay(), beforeCrash, 'DSH hard restart must preserve committed projection');
    const waiting = runtime.prompt('wait for cancellation');
    // No committed update is emitted for an unfinished provider block. ACP cancel is session-scoped.
    await new Promise(resolve => setTimeout(resolve, 500));
    runtime.cancel();
    assert.equal((await waiting).stopReason, 'cancelled');
    console.log(JSON.stringify({ sessionId, projectedEvents: runtime.replay().length, approvals: approvals.length, cutexMcpCalls: calls.length, busPaths: [...new Set(paths)] }));
  } catch (error) {
    // The process environment is keyless; diagnostics contain only this fixture's values.
    console.error(runtime.rpc.stderr);
    throw error;
  }
});
