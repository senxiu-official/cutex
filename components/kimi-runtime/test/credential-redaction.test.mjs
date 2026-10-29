// r266 B1 regression (host-a QA2 canary): a Kimi account's OAuth tokens may be
// read by Kimi's own tools (the model can read its credentials, as with the
// official CLI), but never land in what cutex records or serves: events,
// thread/read, the timeline database, offline history. Also after a refresh
// (generation 2) and for rotated values the adapter no longer holds
// (fingerprints in the account's ledger). Synthetic tokens only.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { randomUUID, createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, writeFileSync, renameSync, readFileSync, rmSync, symlinkSync, readdirSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { redactionSource, secretRedactor, streamRedactor, fingerprintOf, sourceFromEnv } from '../src/redaction.mjs';
import { install, kimiMain, privateKimi, connect, poll, delay } from './harness.mjs';

if (install) process.env.KIMI_INSTALL_ROOT ??= install;

// JWT-shaped synthetic values, as Kimi's OAuth host issues them.
const jwt = label => `eyJhbGciOiJIUzI1NiJ9.${Buffer.from(`{"sub":"canary-${label}-${randomUUID()}"}`).toString('base64url')}.c2lnLWNhbmFyeQ`;
const generation = n => ({ access: jwt(`access${n}`), refresh: jwt(`refresh${n}`) });
const tokenFile = ({ access, refresh }) => JSON.stringify({ access_token: access, refresh_token: refresh, expires_at: Math.floor(Date.now() / 1000) + 3600, scope: 'kimi-code', token_type: 'Bearer', expires_in: 3600 }, null, 2) + '\n';
/** Kimi's FileTokenStorage.save: tmp + rename through the link. */
function kimiSave(dir, gen) {
  const temporary = join(dir, `kimi-code.json.tmp.${process.pid}.${randomUUID().slice(0, 8)}`);
  writeFileSync(temporary, tokenFile(gen), { mode: 0o600 });
  renameSync(temporary, join(dir, 'kimi-code.json'));
}
const values = gen => [gen.access, gen.refresh];
function temp(t, label) {
  const dir = mkdtempSync(join(tmpdir(), `kimi-redact-${label}-`));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test('the source re-reads the credential files after a refresh, keeps earlier values and fingerprints them in the ledger', t => {
  const dir = temp(t, 'source');
  const credentials = join(dir, 'credentials');
  mkdirSync(credentials);
  const ledger = join(dir, 'redaction', 'ka-test.json');
  const [g1, g2, g3] = [generation(1), generation(2), generation(3)];
  kimiSave(credentials, g1);
  const source = redactionSource({ literals: ['launch-token-value-0123'], credentialsDir: credentials, ledger });
  const redact = secretRedactor(source);
  assert.equal(redact({ out: `cat: ${g1.access} ${g1.refresh}` }).out, 'cat: [REDACTED] [REDACTED]');
  kimiSave(credentials, g2);
  const out = redact([`${g2.access}|${g1.refresh}|launch-token-value-0123|scope kimi-code|Bearer`]);
  assert.deepEqual(out, ['[REDACTED]|[REDACTED]|[REDACTED]|scope kimi-code|Bearer'], 'current, retired and fixed values; never scope or type');
  // The ledger holds digests of every value this process saw, never a value.
  const raw = readFileSync(ledger, 'utf8');
  for (const value of [...values(g1), ...values(g2)]) {
    assert.ok(!raw.includes(value));
    assert.ok(JSON.parse(raw).some(fp => fp.sha256 === createHash('sha256').update(value).digest('hex')));
  }
  // Another process (an offline reader) after a third refresh: g1 and g2 only
  // as fingerprints; found inside JSON, after Bearer, in a path, as a value.
  kimiSave(credentials, g3);
  const reader = secretRedactor(sourceFromEnv({ KIMI_CUTEX_REDACT_SOURCES: JSON.stringify({ credentialsDir: credentials, ledger }) }));
  const text = `{"access_token":"${g1.access}"} Authorization: Bearer ${g2.access}\n/tmp/${g2.refresh}/x token=${g1.refresh}& ${g3.access}`;
  const redacted = reader(text);
  for (const value of [...values(g1), ...values(g2), g3.access]) assert.ok(!redacted.includes(value), redacted);
  assert.equal(redacted, '{"access_token":"[REDACTED]"} Authorization: Bearer [REDACTED]\n/tmp/[REDACTED]/x token=[REDACTED]& [REDACTED]');
  // A same-length different value stays.
  const other = g1.access.slice(0, -1) + (g1.access.endsWith('Q') ? 'R' : 'Q');
  assert.equal(reader(other), other);
  assert.equal(fingerprintOf('short'), null);
});

test('streaming: a fingerprinted value split over any two deltas never leaves in pieces', () => {
  const g = generation(9);
  const fp = fingerprintOf(g.refresh);
  const source = { literals: () => [], fingerprints: () => new Map([[`${fp.length}:${fp.sha256}`, fp]]) };
  const factory = streamRedactor(source);
  const text = `the file says "${g.refresh}", done`;
  for (let i = 1; i < text.length; i++) {
    const filter = factory();
    const pieces = [filter.push(text.slice(0, i)), filter.push(text.slice(i)), filter.flush()];
    assert.equal(pieces.join(''), 'the file says "[REDACTED]", done', `split ${i}`);
    for (const piece of pieces) {
      for (let at = 0; at + 12 <= piece.length; at++) assert.ok(!g.refresh.includes(piece.slice(at, at + 12)), `split ${i}: ${piece}`);
    }
  }
});

const turnDone = (client, turnId) => poll(() => client.notifications.find(n => n.method === 'turn/completed' && n.params.turn.id === turnId), Boolean, `turn ${turnId} completed`, 60000);

test('canary: Read and a shell command print the linked account credentials; nothing cutex records or serves holds a token, before and after a refresh, live and offline', { skip: !install, timeout: 180000 }, async t => {
  const { startServer } = await import('../src/server.mjs');
  const kimi = await privateKimi(t, { label: 'canary' });
  // The account home's credentials/, linked into the agent's KIMI_CODE_HOME as cutex does.
  const account = join(kimi.dirs.root, 'account');
  mkdirSync(join(account, 'credentials'), { recursive: true });
  symlinkSync(join(account, 'credentials'), join(kimi.dirs.kimiHome, 'credentials'), process.platform === 'win32' ? 'junction' : 'dir');
  const linked = join(kimi.dirs.kimiHome, 'credentials');
  const ledger = join(kimi.dirs.root, 'runtime', 'kimi', 'redaction', 'ka-canary.json');
  const [g1, g2] = [generation(1), generation(2)];
  kimiSave(linked, g1);
  const threadId = randomUUID();
  const config = {
    command: process.execPath, args: [kimiMain, 'acp'], cwd: kimi.dirs.cwd, env: kimi.env, kimiHome: kimi.dirs.kimiHome,
    database: join(kimi.dirs.state, 'timeline.sqlite'), readyFile: join(kimi.dirs.state, 'ready.json'), token: `tok-${randomUUID()}-${randomUUID()}`,
    threadId, runtimeId: `kimi.${randomUUID()}`, model: 'mock/k', liveness: { interval: 1000, probeTimeout: 1000 }, stopGrace: 3000,
    redaction: { credentialsDir: linked, ledger },
  };
  const server = await startServer(config);
  let closed = false;
  kimi.onClose(() => closed ? undefined : server.close());
  const client = await connect(server.endpoint, config.token);
  kimi.onClose(() => client.close());
  await client.request('thread/start', {});
  const file = join(linked, 'kimi-code.json');
  const cat = process.platform === 'win32' ? `type "${file}"` : `cat '${file}'`;
  const canaryTurn = async () => {
    const steps = [{ name: 'Read', args: { path: file } }, { name: 'Bash', args: { command: cat } }];
    const turnId = (await client.request('turn/start', { threadId, input: [{ type: 'text', text: `STEPS ${JSON.stringify(steps)}` }] })).turn.id;
    // Approve whatever Kimi asks (the owner lets the agent read its credentials).
    const answered = new Set();
    const pump = setInterval(() => { for (const r of client.requests) if (!answered.has(r.id)) { answered.add(r.id); client.respond(r.id, { decision: 'accept' }); } }, 50);
    try { await turnDone(client, turnId); } finally { clearInterval(pump); }
    return turnId;
  };
  const leaks = (label, text, secrets) => secrets.filter(secret => text.includes(secret)).map(() => label);
  const check = async secrets => {
    const found = [];
    found.push(...leaks('events', JSON.stringify(client.notifications), secrets));
    found.push(...leaks('requests', JSON.stringify(client.requests), secrets));
    found.push(...leaks('query', JSON.stringify(await client.request('thread/read', { threadId, includeTurns: true })), secrets));
    found.push(...leaks('items', JSON.stringify(await client.request('thread/items/list', { threadId, limit: 200 }).catch(() => ({}))), secrets));
    for (const name of readdirSync(kimi.dirs.state)) found.push(...leaks(`state/${name}`, readFileSync(join(kimi.dirs.state, name)).toString('latin1'), secrets));
    found.push(...leaks('ledger', existsSync(ledger) ? readFileSync(ledger, 'utf8') : '', secrets));
    return found;
  };
  const first = await canaryTurn();
  // The tools did read the token (the witness: Kimi's own session log has it).
  const read = await client.request('thread/read', { threadId, includeTurns: true });
  const items = read.thread.turns.find(turn => turn.id === first).items;
  assert.ok(items.filter(item => item.type === 'commandExecution').every(item => item.status === 'completed'), JSON.stringify(items).slice(0, 2000));
  assert.ok(JSON.stringify(items).includes('[REDACTED]'), 'the token was in the tool results and was redacted');
  assert.deepEqual(await check(values(g1)), []);
  // Refresh (generation 2): Kimi's save through the link; the adapter re-reads.
  kimiSave(linked, g2);
  await canaryTurn();
  assert.deepEqual(await check([...values(g1), ...values(g2)]), []);
  // Offline: the history reader of a stopped agent (a new process) redacts
  // from the files and the ledger; generation 1 only by fingerprint now.
  await client.close();
  await server.close();
  closed = true;
  kimiSave(linked, generation(3));
  const offlineToken = `tok-${randomUUID()}-${randomUUID()}`;
  const offline = await startServer({ readOnly: true, cwd: kimi.dirs.cwd, database: config.database, readyFile: join(kimi.dirs.state, 'ready-offline.json'),
    token: offlineToken, threadId, redaction: { credentialsDir: linked, ledger } });
  kimi.onClose(() => offline.close());
  const reader = await connect(offline.endpoint, offlineToken);
  kimi.onClose(() => reader.close());
  const history = JSON.stringify(await reader.request('thread/read', { threadId, includeTurns: true }));
  assert.ok(history.includes('[REDACTED]'));
  for (const value of [...values(g1), ...values(g2)]) assert.ok(!history.includes(value), 'offline history');
  await delay(10);
});
