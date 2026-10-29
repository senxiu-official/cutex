// Real-DSH proof of cutex's DSH revert and fork (src/session-ops.mjs + the
// renames cutex's dsh_structure.rs performs): DSH 0.2.1-rc.3 resumes its own log
// cut after a kept turn (same native session ID, same kept turn IDs) and the
// dropped turns are gone from the model context; a fork copy resumes in another
// home under a new ID. Keyless and offline (fixture LLM).
// Run: DSH_SPIKE_INSTALL=<install root> node --test test/real-dsh-session-ops.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rename, rm } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { DshRuntime } from '../src/runtime.mjs';
import { locateNativeLog } from '../src/reconcile.mjs';
import { revertPrepare, forkCopy } from '../src/session-ops.mjs';

const install = process.env.DSH_SPIKE_INSTALL;
const plugin = fileURLToPath(new URL('./native-crash-worker.mjs', import.meta.url));

test('real DSH resumes a reverted log (same identity) and a fork copy in another home', { skip: !install, timeout: 300000 }, async t => {
  const installRoot = resolve(install);
  const base = await mkdtemp(join(tmpdir(), 'dsh-ops-'));
  t.after(() => rm(base, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 }));
  const audit = join(base, 'provider-audit.jsonl');
  const cwd = join(base, 'workspace');
  await mkdir(cwd);
  await writeFile(join(base, '.cutex-test-private-home'), 'keyless real DSH session-ops fixture\n');
  const yamlPath = value => value.replaceAll('\\', '/').replaceAll("'", "''");
  const agent = async name => {
    const home = join(base, name);
    await mkdir(home);
    await writeFile(join(home, 'offline.patch.yml'), `
- id: llm-deepseek
  disabled: true
- id: acp
  config:
    provider: native-crash-offline
    model: offline
- id: session-persistence-jsonl
  config:
    root: '${yamlPath(join(home, 'sessions'))}'
    compression: none
- id: session-log-deepseek
  config:
    enabled: false
- insert:
    - id: native-crash-offline-fixture
      name: '${yamlPath(plugin)}'
`);
    return { home, sessionsRoot: join(home, 'sessions'), database: join(home, 'timeline.sqlite') };
  };
  const env = home => Object.assign(Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT'].filter(key => process.env[key]).map(key => [key, process.env[key]])),
    { HOME: home, USERPROFILE: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'), TEMP: home, TMP: home,
      CUTEX_TEST_PRIVATE_HOME: base, DSH_HOME: join(home, 'dsh'), DSH_TELEMETRY_DISABLED: '1', DSH_SPIKE_INSTALL_ROOT: installRoot, DSH_NATIVE_CRASH_AUDIT: audit });
  const runtime = a => new DshRuntime({
    command: process.execPath, args: [join(installRoot, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), '--profile', 'acp', '--patch', join(a.home, 'offline.patch.yml')],
    cwd, env: env(a.home), database: a.database, sessionsRoot: a.sessionsRoot, installRoot,
  });
  const lastRequest = async () => JSON.parse((await readFile(audit, 'utf8')).trim().split('\n').at(-1));
  const turns = r => r.store.turnIds();

  const source = await agent('source');
  let r = runtime(source), sessionId, before;
  try {
    await r.initialize();
    sessionId = r.sessionId;
    for (const text of ['FIRST', 'SECOND', 'THIRD']) await r.prompt(text);
    before = turns(r);
  } finally { await r.close(); }
  assert.deepEqual(before, [1, 2, 3].map(n => `${sessionId}:turn:${n}`));
  const kept = (await lastRequest()).userMessages.slice(0, 1);

  // Revert before turn 2, swapped in the way dsh_structure.rs does it.
  const staging = join(base, 'staged');
  const prepared = await revertPrepare({ database: source.database, sessionsRoot: source.sessionsRoot, sessionId, cwd, beforeTurnId: before[1], stagingDirectory: staging, installRoot });
  assert.deepEqual(prepared.removedTurnIds, [before[2], before[1]]);
  await rename(source.database, join(base, 'old-timeline.sqlite'));
  for (const suffix of ['-wal', '-shm']) if (existsSync(source.database + suffix)) await rename(source.database + suffix, join(base, `old-timeline.sqlite${suffix}`));
  await rename(prepared.stagedDatabase, source.database);
  await rename(prepared.stagedLog, prepared.logPath);
  r = runtime(source);
  try {
    await r.initialize();
    assert.equal(r.sessionId, sessionId, 'DSH resumes the same native session');
    assert.deepEqual(turns(r), [before[0]], 'the kept turn keeps its ID');
    await r.prompt('FOURTH_AFTER_REVERT');
    assert.deepEqual(turns(r), [before[0], `${sessionId}:turn:2`]);
    assert.equal(r.store.nativeStatus(sessionId)?.state ?? 'ok', 'ok');
  } finally { await r.close(); }
  const afterRevert = (await lastRequest()).userMessages;
  assert.equal(afterRevert.length, 2, 'the provider sees only the kept prompt and the new one');
  assert.equal(afterRevert[0], kept[0]);

  // Fork after the (new) turn 2 into another agent home under a new ID.
  const child = await agent('child');
  const newSessionId = randomUUID();
  const copied = await forkCopy({ database: source.database, sessionsRoot: source.sessionsRoot, sessionId, cwd, afterTurnId: null,
    targetSessionsRoot: child.sessionsRoot, newSessionId, targetDatabase: child.database, installRoot });
  assert.equal(copied.forkedThroughTurnId, `${sessionId}:turn:2`);
  assert.ok(await locateNativeLog(child.sessionsRoot, newSessionId));
  r = runtime(child);
  try {
    await r.initialize();
    assert.equal(r.sessionId, newSessionId);
    assert.deepEqual(turns(r), [`${newSessionId}:turn:1`, `${newSessionId}:turn:2`]);
    await r.prompt('FIRST_IN_FORK');
    assert.equal(turns(r).length, 3);
  } finally { await r.close(); }
  const forkRequest = (await lastRequest()).userMessages;
  assert.deepEqual(forkRequest.slice(0, 2), afterRevert, 'the fork continues from the copied conversation');
  assert.equal(forkRequest.length, 3);

  // Revert everything: only the session settings rows remain, DSH still resumes it.
  const all = await revertPrepare({ database: child.database, sessionsRoot: child.sessionsRoot, sessionId: newSessionId, cwd,
    beforeTurnId: `${newSessionId}:turn:1`, stagingDirectory: join(base, 'staged-all'), installRoot });
  assert.equal(all.removedTurnIds.length, 3);
  await rm(child.database, { force: true });
  for (const suffix of ['-wal', '-shm']) await rm(child.database + suffix, { force: true });
  await rename(all.stagedDatabase, child.database);
  await rename(all.stagedLog, all.logPath);
  r = runtime(child);
  try {
    await r.initialize();
    assert.equal(r.sessionId, newSessionId);
    assert.deepEqual(turns(r), []);
    await r.prompt('FIRST_AFTER_FULL_REVERT');
    assert.deepEqual(turns(r), [`${newSessionId}:turn:1`]);
  } finally { await r.close(); }
  assert.equal((await lastRequest()).userMessages.length, 1);
});
