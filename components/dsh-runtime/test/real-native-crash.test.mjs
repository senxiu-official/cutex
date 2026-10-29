import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fork } from 'node:child_process';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { TimelineStore } from '../src/store.mjs';
import { loadNativeCatalog, locateNativeLog, readNativeLog } from '../src/reconcile.mjs';

const install = process.env.DSH_SPIKE_INSTALL;
const workerPath = fileURLToPath(new URL('./native-crash-worker.mjs', import.meta.url));
const marker = 'NATIVE_DURABLE_ASSISTANT_CRASH_WINDOW';
const assistantItems = timeline => timeline.filter(row => row.method === 'item/completed' && row.params.item.type === 'agentMessage');

test('real DSH durable assistant survives death before native projection commit and is never resent', {
  skip: !install, timeout: 300000,
}, async t => {
  const installRoot = resolve(install);
  const base = join(homedir(), '.cutex-dsh-spike');
  await mkdir(base, { recursive: true });
  const home = await mkdtemp(join(base, 'native-crash-'));
  const sessionsRoot = join(home, 'sessions');
  const auditPath = join(home, 'provider-audit.jsonl');
  await mkdir(join(home, 'workspace'));
  await writeFile(join(home, '.cutex-test-private-home'), 'keyless real native crash fixture\n');
  // Deliberate allowlist: no owner credentials, provider keys, inherited NODE_OPTIONS,
  // user DSH configuration or live daemon addresses reach either child process.
  const env = Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT']
    .filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'),
    TEMP: home, TMP: home, CUTEX_TEST_PRIVATE_HOME: home, DSH_HOME: join(home, 'dsh'), DSH_TELEMETRY_DISABLED: '1',
    DSH_SPIKE_INSTALL_ROOT: installRoot, DSH_NATIVE_CRASH_AUDIT: auditPath });
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
      name: '${yamlPath(workerPath)}'
`);
  const workers = new Set();
  const dshPids = new Set();
  const killDsh = pid => {
    if (!dshPids.delete(pid)) return;
    try { process.kill(pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
  };
  t.after(async () => {
    // No image-name or process-tree kills: only our fork handles and the exact
    // DSH child PIDs they report immediately after constructing the runtime.
    for (const worker of workers) if (worker.exitCode === null && worker.signalCode === null) worker.kill('SIGKILL');
    for (const pid of [...dshPids]) killDsh(pid);
    await Promise.all([...workers].map(worker => worker.exitCode !== null || worker.signalCode !== null
      ? undefined : once(worker, 'exit')));
    await rm(home, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  });
  function launch(mode) {
    const worker = fork(workerPath, [mode, home, installRoot], { env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe', 'ipc'] });
    workers.add(worker);
    const messages = [];
    let stderr = '';
    worker.stderr.on('data', bytes => { stderr = (stderr + bytes).slice(-32768); });
    worker.stdout.resume();
    worker.on('message', message => {
      messages.push(message);
      if (message.type === 'spawned') dshPids.add(message.dshPid);
    });
    return { worker, async wait(type) {
      const deadline = Date.now() + (type === 'checkpoint' || type === 'recovered' ? 180000 : 30000);
      while (Date.now() < deadline) {
        const error = messages.find(message => message.type === 'error');
        if (error) throw new Error(`${error.message}\n${error.stderr}`);
        const message = messages.find(message => message.type === type);
        if (message) return message;
        if (worker.exitCode !== null || worker.signalCode !== null) throw new Error(`Worker exited waiting for ${type}: ${stderr}`);
        await delay(20);
      }
      throw new Error(`Timed out waiting for ${type}: ${stderr}`);
    } };
  }
  const crashing = launch('crash');
  const checkpoint = await crashing.wait('checkpoint');
  assert.equal(assistantItems(checkpoint.timelineBefore).length, 0, 'assistant has not reached projected timeline');
  assert.ok(checkpoint.cursorBefore.nativeSeq < checkpoint.nativeSeq);
  assert.equal((await crashing.wait('prompt-settled')).stopReason, 'end_turn', 'DSH completes independently of blocked projection');

  const logPath = await locateNativeLog(sessionsRoot, checkpoint.sessionId);
  assert.ok(logPath);
  const catalog = await loadNativeCatalog(join(installRoot, 'package.json'));
  let durable;
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    durable = await readNativeLog({ logPath, sessionId: checkpoint.sessionId, catalog });
    if (durable.events.some(record => record.event.type === 'turn/end')) break;
    await delay(25);
  }
  assert.ok(durable.events.some(record => record.event.type === 'turn/end'), 'DSH durable writer flushed the terminal event');
  const nativeAssistant = durable.events.filter(record => record.event.type === 'assistant/message');
  assert.equal(nativeAssistant.length, 1);
  assert.equal(nativeAssistant[0].nativeSeq, checkpoint.nativeSeq);
  assert.equal(nativeAssistant[0].event.data.message.id, checkpoint.messageId);
  assert.equal(nativeAssistant[0].event.data.message.content.find(block => block.type === 'text').text, marker);
  const providerBefore = await readFile(auditPath, 'utf8');
  assert.equal(providerBefore.trim().split('\n').length, 1);

  const workerExit = once(crashing.worker, 'exit');
  crashing.worker.kill('SIGKILL');
  killDsh(checkpoint.dshPid);
  await workerExit;
  // Inspect production SQLite after the controller death, before recovery.
  const before = new TimelineStore(join(home, 'timeline.sqlite'), { readOnly: true });
  try {
    assert.equal(assistantItems(before.replay()).length, 0);
    assert.ok(before.nativeCursor(checkpoint.sessionId).nativeSeq < checkpoint.nativeSeq);
  } finally { before.close(); }

  const resumed = launch('resume');
  const recovered = await resumed.wait('recovered');
  assert.equal(recovered.sessionId, checkpoint.sessionId);
  assert.deepEqual(recovered.second, recovered.first, 'repeated scans cannot duplicate projections');
  assert.equal(assistantItems(recovered.first).length, 1);
  assert.equal(assistantItems(recovered.first)[0].params.item.text, marker);
  assert.equal(assistantItems(recovered.emitted).length, 1, 'recovery emits exactly one committed assistant item');
  assert.equal(recovered.first.filter(row => row.method === 'turn/started').length, 1);
  assert.equal(recovered.first.filter(row => row.method === 'turn/completed').length, 1);
  assert.ok(recovered.methods.includes('session/resume'));
  assert.equal(recovered.methods.includes('session/prompt'), false, 'recovery must not resend prompt');
  assert.ok(recovered.cursor.nativeSeq >= checkpoint.nativeSeq);
  assert.equal(await readFile(auditPath, 'utf8'), providerBefore, 'resume invokes no provider request');
  await resumed.wait('closed');
  // Graceful runtime.close has reaped this worker's DSH process already.
  const spawned = await resumed.wait('spawned');
  dshPids.delete(spawned.dshPid);
  console.log(JSON.stringify({ sessionId: checkpoint.sessionId, nativeAssistantSeq: checkpoint.nativeSeq,
    projectedAssistantCopies: 1, providerRequests: 1, recoveredEvents: recovered.first.length }));
});
