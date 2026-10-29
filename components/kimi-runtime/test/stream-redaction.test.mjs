// Chunk-safe redaction (r265b parity): a secret split over streamed deltas
// never leaves in pieces; retired keys cutex passes in DSH_CUTEX_REDACT /
// KIMI_CUTEX_REDACT are redacted too and never reach Kimi's environment.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { DatabaseSync } from 'node:sqlite';
import { streamRedactor } from '../src/history-reader.mjs';
import { install, kimiMain, privateKimi, connect, poll } from './harness.mjs';

if (install) process.env.KIMI_INSTALL_ROOT ??= install;

const SECRET = 'sk-fake-kimi-retired-0123456789abcdef';

const run = (filter, chunks) => {
  const out = chunks.map(chunk => filter.push(chunk));
  out.push(filter.flush());
  return out;
};

test('streamRedactor: every 2-way and 3-way split of a secret is redacted, no piece carries part of it', () => {
  const factory = streamRedactor([SECRET, 'other-secret-value']);
  const text = `before ${SECRET} after`;
  const leaks = pieces => pieces.some(piece => piece.length > 2 && SECRET.includes(piece) && piece !== '[REDACTED]');
  for (let i = 1; i < text.length; i++) {
    const out = run(factory(), [text.slice(0, i), text.slice(i)]);
    assert.equal(out.join(''), 'before [REDACTED] after', `split ${i}`);
    assert.ok(!out.some(piece => piece.includes(SECRET.slice(0, 6))), `split ${i}: ${JSON.stringify(out)}`);
  }
  for (let i = 1; i < text.length; i += 3) {
    for (let j = i + 1; j < text.length; j += 5) {
      const out = run(factory(), [text.slice(0, i), text.slice(i, j), text.slice(j)]);
      assert.equal(out.join(''), 'before [REDACTED] after', `split ${i}/${j}`);
    }
  }
  const single = run(factory(), [...text]);
  assert.equal(single.join(''), 'before [REDACTED] after');
  assert.ok(!leaks(single));
  // A prefix of a secret that never completes is not lost.
  assert.equal(run(factory(), ['ends with sk-fake-ki', 'm']).join(''), 'ends with sk-fake-kim');
  assert.equal(run(factory(), ['ends with sk-fake-ki']).join(''), 'ends with sk-fake-ki');
  // Two secrets in one stream.
  assert.equal(run(factory(), ['a ', SECRET.slice(0, 9), SECRET.slice(9), ' b other-sec', 'ret-value c']).join(''), 'a [REDACTED] b [REDACTED] c');
});

test('live Kimi deltas, the stored item and the timeline never carry a split retired key; Kimi never sees it', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'redact' });
  const { startServer, configFromEnv } = await import('../src/server.mjs');
  // The launch path: cutex passes earlier keys in the adapter's own environment.
  const dir = mkdtempSync(join(tmpdir(), 'kimi-redact-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const file = join(dir, 'launch.json');
  const threadId = randomUUID();
  const token = `tok-${randomUUID()}-${randomUUID()}`;
  writeFileSync(file, JSON.stringify({
    command: process.execPath, args: [kimiMain, 'acp'], cwd: kimi.dirs.cwd, env: kimi.env, kimiHome: kimi.dirs.kimiHome,
    database: join(kimi.dirs.state, 'timeline.sqlite'), readyFile: join(kimi.dirs.state, 'ready.json'),
    threadId, runtimeId: `kimi.${randomUUID()}`, model: 'mock/k',
  }));
  const config = configFromEnv({ KIMI_CUTEX_CONFIG: file, KIMI_CUTEX_TOKEN: token, DSH_CUTEX_REDACT: JSON.stringify([SECRET]) });
  assert.deepEqual(config.redact, [SECRET]);
  assert.ok(!Object.values(config.env).includes(SECRET), 'retired keys never enter Kimi’s environment');
  const server = await startServer({ ...config, liveness: { interval: 1000, probeTimeout: 1000 }, stopGrace: 3000 });
  kimi.onClose(() => server.close());
  const client = await connect(server.endpoint, token);
  kimi.onClose(() => client.close());
  await client.request('thread/start', {});
  // The mock echoes in 8-character pieces: the key spans several deltas.
  const turnId = (await client.request('turn/start', { threadId, input: [{ type: 'text', text: `say ${SECRET} now` }] })).turn.id;
  const done = await poll(() => client.notifications.find(n => n.method === 'turn/completed' && n.params.turn.id === turnId), Boolean, 'turn completed', 60000);
  assert.equal(done.params.turn.status, 'completed');
  const deltas = client.notifications.filter(n => n.method === 'item/agentMessage/delta' && n.params.turnId === turnId).map(n => n.params.delta);
  assert.ok(deltas.length > 2, JSON.stringify(deltas));
  assert.equal(deltas.join(''), 'echo: say [REDACTED] now');
  assert.ok(!deltas.some(delta => delta.includes(SECRET.slice(0, 8)) || delta.includes(SECRET.slice(-8))), JSON.stringify(deltas));
  const read = await client.request('thread/read', { threadId, includeTurns: true });
  const turn = read.thread.turns.find(entry => entry.id === turnId);
  assert.equal(turn.items.find(item => item.type === 'agentMessage').text, 'echo: say [REDACTED] now');
  await server.close();
  const db = new DatabaseSync(join(kimi.dirs.state, 'timeline.sqlite'), { readOnly: true });
  const bodies = db.prepare('SELECT body FROM events').all().map(row => row.body).join('\n');
  db.close();
  assert.ok(!bodies.includes(SECRET), 'timeline holds no secret');
});
