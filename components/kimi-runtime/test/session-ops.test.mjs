// Revert and fork on real Kimi session logs (pinned install, offline mock).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { renameSync, mkdirSync, writeFileSync, existsSync } from 'node:fs';
import { install, kimiMain, privateKimi, connect, poll } from './harness.mjs';

if (install) process.env.KIMI_INSTALL_ROOT ??= install;
const { startServer } = install ? await import('../src/server.mjs') : {};
const ops = install ? await import('../src/session-ops.mjs') : {};

async function adapter(kimi, { kimiHome = kimi.dirs.kimiHome, state = kimi.dirs.state, threadId = randomUUID() } = {}) {
  const config = { command: process.execPath, args: [kimiMain, 'acp'], cwd: kimi.dirs.cwd, env: { ...kimi.env, KIMI_CODE_HOME: kimiHome }, kimiHome,
    database: join(state, 'timeline.sqlite'), readyFile: join(state, 'ready.json'), token: `tok-${randomUUID()}-${randomUUID()}`, threadId,
    runtimeId: `kimi.${randomUUID()}`, model: 'mock/k', stopGrace: 3000 };
  const server = await startServer(config);
  const client = await connect(server.endpoint, config.token);
  kimi.onClose(async () => { client.close(); await server.close(); });
  await client.request('thread/start', {});
  const say = async text => {
    const turnId = (await client.request('turn/start', { threadId, input: [{ type: 'text', text }] })).turn.id;
    await poll(() => client.notifications.find(n => n.method === 'turn/completed' && n.params.turn.id === turnId), Boolean, `turn ${text}`, 60000);
    return { turnId, text: client.notifications.filter(n => n.method === 'item/agentMessage/delta' && n.params.turnId === turnId).map(n => n.params.delta).join('') };
  };
  return { server, client, threadId, say, sessionId: () => server.ready?.nativeSessionId };
}

test('revert cuts Kimi\'s own context and the timeline; fork copies a prefix into another home', { skip: !install, timeout: 240000 }, async t => {
  const kimi = await privateKimi(t, { label: 'ops' });
  const a = await adapter(kimi);
  const one = await a.say('first message');
  const two = await a.say('second message');
  await a.say('third message');
  const status = await a.client.request('dsh/runtime/status', { threadId: a.threadId });
  const sessionId = status.nativeSessionId;
  await a.server.close();

  // Fork after the first turn into a new private home (the source may run; here it is stopped).
  const childRoot = join(kimi.dirs.root, 'child');
  const childHome = join(childRoot, 'kimi');
  mkdirSync(childHome, { recursive: true });
  writeFileSync(join(childHome, 'config.toml'), (await import('node:fs')).readFileSync(join(kimi.dirs.kimiHome, 'config.toml')));
  const fork = ops.forkCopy({ database: join(kimi.dirs.state, 'timeline.sqlite'), sessionsRoot: join(kimi.dirs.kimiHome, 'sessions'), sessionId, cwd: kimi.dirs.cwd,
    afterTurnId: one.turnId, targetSessionsRoot: join(childHome, 'sessions'), newSessionId: randomUUID(), targetDatabase: join(childRoot, 'state', 'timeline.sqlite') });
  assert.equal(fork.forkedThroughTurnId, one.turnId);
  assert.match(fork.nativeSessionId, /^session_/);

  // Revert before the second turn (runtime stopped), swapped in the way cutex does.
  const staging = join(kimi.dirs.state, '.revert');
  const prepared = ops.revertPrepare({ database: join(kimi.dirs.state, 'timeline.sqlite'), sessionsRoot: join(kimi.dirs.kimiHome, 'sessions'), sessionId,
    cwd: kimi.dirs.cwd, beforeTurnId: two.turnId, stagingDirectory: staging });
  assert.deepEqual(prepared.keptTurnIds, [one.turnId]);
  assert.equal(prepared.removedTurnIds.length, 2);
  for (const suffix of ['', '-wal', '-shm']) if (existsSync(join(kimi.dirs.state, `timeline.sqlite${suffix}`))) renameSync(join(kimi.dirs.state, `timeline.sqlite${suffix}`), join(staging, `old${suffix}`));
  renameSync(prepared.stagedDatabase, join(kimi.dirs.state, 'timeline.sqlite'));
  renameSync(prepared.stagedLog, prepared.logPath);

  const reverted = await adapter(kimi, { threadId: a.threadId });
  const history = await reverted.client.request('thread/read', { threadId: a.threadId, includeTurns: true });
  assert.deepEqual(history.thread.turns.map(turn => turn.id), [one.turnId]);
  const context = await reverted.say('CONTEXT?');
  assert.match(context.text, /user turns: 2 \[first message \| CONTEXT\?\]/, context.text);
  // Usage is not counted again from the cut log.
  const usage = reverted.client.notifications.filter(n => n.method === 'thread/tokenUsage/updated');
  assert.equal(usage.length, 1, JSON.stringify(usage.map(n => n.params.tokenUsage.last)));
  assert.equal(usage[0].params.tokenUsage.total.inputTokens, 400, 'total continues: 3 turns + this one');

  // The fork resumes in its own home with the copied prefix.
  const child = await adapter(kimi, { kimiHome: childHome, state: join(childRoot, 'state'), threadId: randomUUID() });
  const childHistory = await child.client.request('thread/read', { threadId: child.threadId, includeTurns: true });
  assert.equal(childHistory.thread.turns.length, 1);
  assert.notEqual(childHistory.thread.turns[0].id, one.turnId, 'the child has its own turn ids');
  assert.equal(childHistory.thread.turns[0].items.find(item => item.type === 'agentMessage').text, 'echo: first message');
  const childContext = await child.say('CONTEXT?');
  assert.match(childContext.text, /user turns: 2 \[first message \| CONTEXT\?\]/, childContext.text);
  assert.equal((await child.client.request('dsh/runtime/status', { threadId: child.threadId })).nativeSessionId, fork.nativeSessionId);
});
