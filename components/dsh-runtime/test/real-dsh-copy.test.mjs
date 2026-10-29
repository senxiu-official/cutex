// Real-DSH proof for the fork/revert plan (docs/development/r265-dsh-history-notes.md):
// an unseeded copy of a closed log prefix is accepted by DSH's ACP session/resume,
// carries the copied history into the next provider request, and keeps going.
// Keyless and offline (fixture LLM). Run: DSH_SPIKE_INSTALL=<install root> node --test test/real-dsh-copy.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { DshRuntime } from '../src/runtime.mjs';
import { TimelineStore } from '../src/store.mjs';
import { loadNativeCatalog, locateNativeLog, readNativeLog, writeNativeSessionCopy, deleteNativeSession } from '../src/reconcile.mjs';

const install = process.env.DSH_SPIKE_INSTALL;
const plugin = fileURLToPath(new URL('./native-crash-worker.mjs', import.meta.url));

test('real DSH resumes an unseeded copy of a closed prefix (fork/revert building block) and deletes natively', {
  skip: !install, timeout: 300000,
}, async t => {
  const installRoot = resolve(install);
  const home = await mkdtemp(join(tmpdir(), 'dsh-copy-'));
  t.after(() => rm(home, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }));
  const sessionsRoot = join(home, 'sessions');
  const audit = join(home, 'provider-audit.jsonl');
  await mkdir(join(home, 'workspace'));
  await writeFile(join(home, '.cutex-test-private-home'), 'keyless real DSH copy fixture\n');
  const yamlPath = value => value.replaceAll('\\', '/').replaceAll("'", "''");
  await writeFile(join(home, 'offline.patch.yml'), `
- id: llm-deepseek
  disabled: true
- id: acp
  config:
    provider: native-crash-offline
    model: offline
- id: session-persistence-jsonl
  config:
    root: '${yamlPath(sessionsRoot)}'
    compression: none
- id: session-log-deepseek
  config:
    enabled: false
- insert:
    - id: native-crash-offline-fixture
      name: '${yamlPath(plugin)}'
`);
  const env = Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT'].filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'), TEMP: home, TMP: home,
    CUTEX_TEST_PRIVATE_HOME: home, DSH_HOME: join(home, 'dsh'), DSH_TELEMETRY_DISABLED: '1', DSH_SPIKE_INSTALL_ROOT: installRoot, DSH_NATIVE_CRASH_AUDIT: audit });
  const runtime = database => new DshRuntime({
    command: process.execPath, args: [join(installRoot, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), '--profile', 'acp', '--patch', join(home, 'offline.patch.yml')],
    cwd: join(home, 'workspace'), env, database, sessionsRoot, installRoot,
  });
  const requests = async () => (await readFile(audit, 'utf8')).trim().split('\n').map(line => JSON.parse(line));

  const source = runtime(join(home, 'source.sqlite'));
  let sourceId;
  try {
    await source.initialize();
    sourceId = source.sessionId;
    await source.prompt('FIRST_TURN');
    await source.prompt('SECOND_TURN');
  } finally { await source.close(); }
  const catalog = await loadNativeCatalog(join(installRoot, 'package.json'));
  const { events } = await readNativeLog({ logPath: await locateNativeLog(sessionsRoot, sourceId), sessionId: sourceId, catalog });
  const firstEnd = events.find(record => record.event.type === 'turn/end').nativeSeq;
  const firstUser = events.find(record => record.event.type === 'user/message').event.data.id;
  const secondUser = events.filter(record => record.event.type === 'user/message')[1].event.data.id;

  // Revert-as-copy: keep turn 1, drop turn 2, under a new native identity.
  const copyId = randomUUID();
  const copy = await writeNativeSessionCopy({ sessionsRoot, sourceSessionId: sourceId, newSessionId: copyId, throughSeq: firstEnd, catalog });
  assert.equal(copy.events, firstEnd + 1);
  await assert.rejects(writeNativeSessionCopy({ sessionsRoot, sourceSessionId: sourceId, newSessionId: randomUUID(), throughSeq: firstEnd - 1, catalog }), /turn\/end/);
  const database = join(home, 'copy.sqlite');
  const bind = new TimelineStore(database);
  bind.bind(copyId, join(home, 'workspace'));
  bind.close();
  const resumed = runtime(database);
  try {
    await resumed.initialize();
    assert.equal(resumed.sessionId, copyId);
    const projected = resumed.replay();
    assert.equal(projected.filter(event => event.method === 'turn/completed').length, 1, 'copied history projects into the new timeline');
    await resumed.prompt('THIRD_TURN_AFTER_REVERT');
    assert.equal(resumed.replay().filter(event => event.method === 'turn/completed').length, 2);
  } finally { await resumed.close(); }
  const last = (await requests()).at(-1);
  assert.ok(last.userMessages.includes(firstUser), 'the provider sees the kept turn');
  assert.equal(last.userMessages.includes(secondUser), false, 'the dropped turn is gone from the model context');
  assert.equal(last.userMessages.length, 2);

  // Native delete of the source after its runtime stopped.
  const removed = await deleteNativeSession({ sessionsRoot, sessionId: sourceId, database: join(home, 'source.sqlite') });
  assert.ok(removed.removed.length >= 2);
  assert.equal(await locateNativeLog(sessionsRoot, sourceId), null);
  assert.ok(await locateNativeLog(sessionsRoot, copyId));
});
