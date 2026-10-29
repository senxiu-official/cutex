// r266 controls on ACP agents (DSH, Kimi): the scenario shared by
// stop-pause-daemon.test.mjs (real DSH) and tests/r266c-kimi-stop-pause.test.mjs
// (real Kimi CLI). host-a QA r265b-kimi B2/B3:
//  1. owner stop of a running delegation (POST /v2/delegations/{id}/stop ->
//     StopAttempt -> exact interrupt) ends exactly that turn over ACP, same
//     runtime occurrence, never replayed;
//  2. project pause in interrupt mode (ProjectPauseInterrupt -> exact
//     interrupt) ends the member's running turn;
//  3. delegations sent while the project is paused are held; resume delivers
//     each exactly once;
//  4. a delegation submit racing a project pause: after the pause returns no
//     new member turn starts until resume; every ask runs exactly once.
// `hold(marker)` is model input that runs until cancelled; `delegateCall(args)`
// makes the Director's model call agent_delegate.
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { poll, userTexts } from './private-daemon.mjs';

// r267: the processes whose command line holds `marker`, and all their descendants.
export function processTree(marker) {
  const table = new Map();
  for (const name of readdirSync('/proc')) {
    if (!/^\d+$/.test(name)) continue;
    try {
      const cmdline = readFileSync(`/proc/${name}/cmdline`, 'utf8').replaceAll('\0', ' ');
      const ppid = Number(readFileSync(`/proc/${name}/stat`, 'utf8').replace(/^.*\) /s, '').split(' ')[1]);
      table.set(Number(name), { ppid, cmdline });
    } catch { /* exited */ }
  }
  const roots = [...table].filter(([, row]) => row.cmdline.includes(marker) && !row.cmdline.includes('node --test')).map(([pid]) => pid);
  const tree = new Set(roots);
  for (let grew = true; grew;) { grew = false; for (const [pid, row] of table) if (!tree.has(pid) && tree.has(row.ppid)) { tree.add(pid); grew = true; } }
  return [...tree].map(pid => ({ pid, cmdline: table.get(pid).cmdline.slice(0, 120) }));
}
const alive = pids => pids.filter(pid => { try { return !/^\S+ \(.*\) Z/.test(readFileSync(`/proc/${pid}/stat`, 'utf8')); } catch { return false; } });
// The delegation's own record after a stop: r266 has no `stopped` execution
// state; a stop ends the attempt `interrupted` with a stop reason.
const askSummary = ask => {
  if (!ask) return null;
  const last = ask.attempt ?? (Array.isArray(ask.attempts) ? ask.attempts.at(-1) : null);
  return { execution: ask.execution, closed: ask.closed ?? null, attempt: last && { state: last.state, interruptReason: last.interruptReason ?? null, stopIntent: last.stopIntent?.outcome ?? null } };
};

const uuid = () => crypto.randomUUID();
const delay = ms => new Promise(done => setTimeout(done, ms));

export async function stopPauseScenario(t, daemon, { a, b, project, runtime, hold, delegateCall, shell, outside }) {
  const { api, history, idle, state } = daemon;
  // r266: owner input is deferred (retryable `admission_deferred`) while the
  // admission authority fence is busy; retry like a client would.
  const send = async (agent, text) => {
    for (let attempt = 0; ; attempt++) {
      try { return await daemon.send(agent, text); }
      catch (error) { if (attempt >= 20 || !/admission_deferred/.test(error.message)) throw error; deferred++; await delay(500); }
    }
  };
  let deferred = 0;
  const results = [];
  const askFor = async marker => {
    const listed = await poll(() => api('/v2/asks'), value => (value.asks ?? []).some(ask => JSON.stringify(ask).includes(marker)), `ask ${marker} exists`, 120000);
    return listed.asks.find(ask => JSON.stringify(ask).includes(marker));
  };
  const askView = async id => (await api(`/v2/asks/${encodeURIComponent(id)}`)).ask;
  const turnsWith = (turns, marker) => turns.filter(turn => userTexts([turn]).some(text => text.includes(marker)));
  const delegate = (marker, title) => send(a, delegateCall({ target: b.id, text: hold(marker), title: `${title} ${marker}` }));

  // 1. Exact stop of a running delegation turn through the r266 stop path.
  {
    await idle(a); await idle(b);
    const marker = `stop-${uuid().slice(0, 8)}`;
    await delegate(marker, 'held for stop');
    const ask = await askFor(marker);
    const running = await poll(() => state(b), value => value.runtime?.activeTurnId, `${runtime}: B runs the held delegation`, 120000);
    const turnId = running.runtime.activeTurnId;
    const generation = running.runtime.runtimeGeneration;
    await delay(1500);
    assert.equal((await state(b)).runtime?.activeTurnId, turnId, 'still the held turn');
    const stopped = await api(`/v2/delegations/${encodeURIComponent(ask.askId ?? ask.id)}/stop`, { actionId: `stop-${uuid()}` });
    const turns = await poll(() => history(b), value => value.some(turn => turn.id === turnId && turn.status !== 'inProgress'), `${runtime}: held turn ended by the stop`, 60000);
    const ended = turns.find(turn => turn.id === turnId);
    assert.equal(ended.status, 'interrupted', JSON.stringify(ended).slice(0, 600));
    const after = await state(b);
    assert.equal(after.runtime?.runtimeGeneration, generation, 'same runtime occurrence (no restart)');
    assert.ok(after.runtime?.appServerConnected, 'B stays online');
    const view = await poll(() => askView(ask.askId ?? ask.id), value => !JSON.stringify(value).includes('"pending"') || JSON.stringify(value).includes('submitted'), 'stop settled', 30000).catch(() => null);
    const outcome = JSON.stringify(view ?? {});
    assert.ok(!/stop_outcome_unknown|stop_unsupported|explicit launch absent/.test(outcome), `stop outcome: ${outcome.slice(0, 1500)}`);
    // Held asks never replay: the stopped task is not delivered again.
    await delay(4000);
    assert.equal(turnsWith(await history(b), marker).length, 1, 'the stopped delegation ran exactly once');
    const settledAsk = await poll(() => askView(ask.askId ?? ask.id), value => value?.execution && !['running', 'admitted', 'queued'].includes(value.execution), 'stopped delegation settled', 30000).catch(() => null);
    results.push({ step: 'exact stop of a running delegation', turn: ended.status, generation, nodeOutcomes: stopped.nodeOutcomes?.map(row => row.phase) ?? stopped.receipt?.status,
      stopResponse: JSON.stringify(stopped).slice(0, 600), delegation: askSummary(settledAsk) });
  }

  // 1b. (r267 B2) Exact stop of a delegation whose turn runs a live shell
  //     command: the turn ends, and the command's process tree is gone.
  if (shell) {
    await idle(a); await idle(b);
    const marker = `shellhold${uuid().slice(0, 8)}`;
    await send(a, delegateCall({ target: b.id, text: shell(marker), title: `shell ${marker}` }));
    const ask = await askFor(`shell ${marker}`);
    const running = await poll(() => state(b), value => value.runtime?.activeTurnId, `${runtime}: B runs the shell delegation`, 120000);
    const turnId = running.runtime.activeTurnId;
    // The member's default policy may ask the owner first: approve, so the command really runs.
    let approvals = 0;
    const tree = await poll(async () => {
      const rows = processTree(marker);
      if (!rows.length) for (const pending of (await state(b)).runtime?.pendingServerRequests ?? []) { await daemon.answer(b, pending, 'accept').catch(() => {}); approvals += 1; }
      return rows;
    }, rows => rows.some(row => /(^|\s|\/)sleep 600/.test(row.cmdline)), `${runtime}: the held shell command runs`, 120000);
    const pids = tree.map(row => row.pid);
    const started = Date.now();
    const stopped = await api(`/v2/delegations/${encodeURIComponent(ask.askId ?? ask.id)}/stop`, { actionId: `stop-${uuid()}` });
    const turns = await poll(() => history(b), value => value.some(turn => turn.id === turnId && turn.status !== 'inProgress'), `${runtime}: shell turn ended by the stop`, 60000);
    const gone = await poll(() => alive(pids), rows => rows.length === 0, `${runtime}: the shell command's processes exited`, 30000);
    const goneMs = Date.now() - started;
    assert.deepEqual(gone, []);
    assert.deepEqual(processTree(marker), [], 'no process of the stopped command is left');
    const ended = turns.find(turn => turn.id === turnId);
    const after = await state(b);
    assert.equal(after.runtime?.runtimeGeneration, running.runtime.runtimeGeneration, 'same runtime occurrence (no restart)');
    const settledAsk = await poll(() => askView(ask.askId ?? ask.id), value => value?.execution && !['running', 'admitted', 'queued'].includes(value.execution), 'shell delegation settled', 30000).catch(() => null);
    await delay(3000);
    assert.equal(turnsWith(await history(b), marker).length, 1, 'the stopped shell delegation ran exactly once');
    results.push({ step: 'exact stop of a delegation running a shell command', turn: ended.status, ownerApprovals: approvals, processesBeforeStop: tree, processesGoneAfterMs: goneMs,
      nodeOutcomes: stopped.nodeOutcomes?.map(row => row.phase) ?? null, delegation: askSummary(settledAsk),
      commandItem: ended.items.filter(item => item.type === 'commandExecution').map(item => ({ status: item.status, exitCode: item.exitCode ?? null })) });
  }

  // 2. Project pause, interrupt mode: the member's running turn ends.
  let pause;
  {
    await idle(b);
    const before = (await history(b)).length;
    await send(b, hold('owner turn to be paused'));
    const running = await poll(() => state(b), value => value.runtime?.activeTurnId, `${runtime}: B runs the held owner turn`, 60000);
    const turnId = running.runtime.activeTurnId;
    const control = await api(`/v2/projects/${encodeURIComponent(project)}/run-control`);
    pause = await api(`/v2/projects/${encodeURIComponent(project)}/pause`, { actionId: `pause-${uuid()}`, expectedEpoch: control.runControl?.epoch ?? control.epoch ?? 0, mode: 'interrupt' });
    const turns = await poll(() => history(b), value => value.some(turn => turn.id === turnId && turn.status !== 'inProgress'), `${runtime}: paused turn interrupted`, 60000);
    assert.equal(turns.find(turn => turn.id === turnId).status, 'interrupted');
    assert.ok(turns.length >= before + 1);
    results.push({ step: 'project pause (interrupt) ends the running member turn', pauseId: pause.runControl?.pauseId ?? pause.pauseId });
  }

  // 3. While paused, delegations are held; resume delivers each exactly once.
  const runControl = async () => { const value = await api(`/v2/projects/${encodeURIComponent(project)}/run-control`); return value.runControl ?? value; };
  const resume = async () => { const control = await runControl(); return api(`/v2/projects/${encodeURIComponent(project)}/resume`, { actionId: `resume-${uuid()}`, pauseId: control.pauseId, expectedEpoch: control.epoch }); };
  const running = marker => poll(() => askFor(marker), ask => ask.execution === 'running' && ask.attempt?.turnId, `${runtime}: ${marker} running`, 120000);
  const stopRunning = async ask => {
    await api(`/v2/delegations/${encodeURIComponent(ask.askId)}/stop`, { actionId: `stop-${uuid()}` });
    await poll(() => history(b), value => value.some(turn => turn.id === ask.attempt.turnId && turn.status !== 'inProgress'), `${runtime}: ${ask.askId} stopped`, 60000);
  };
  {
    await idle(a); await idle(b);
    const bTurnsAtPause = (await history(b)).length;
    const markers = [];
    for (let i = 0; i < 2; i++) {
      const marker = `queued-${i}-${uuid().slice(0, 8)}`;
      markers.push(marker);
      await idle(a);
      await delegate(marker, `held ${i}`);
      await askFor(marker);
    }
    await delay(8000);
    assert.equal((await history(b)).length, bTurnsAtPause, `${runtime}: no turn admitted to the paused member`);
    for (const marker of markers) assert.notEqual((await askFor(marker)).execution, 'running', `${marker} admitted while paused`);
    await resume();
    // Resumed: the held delegations run one after the other (each holds
    // until stopped). Order is not asserted: a transient Busy refusal at the
    // resume boundary requeues an attempt.
    const order = [], done = new Set();
    for (let i = 0; i < markers.length; i++) {
      const ask = await poll(async () => (await api('/v2/asks')).asks.find(row => !done.has(row.askId) && markers.some(marker => row.title?.includes(marker)) && row.execution === 'running' && row.attempt?.turnId),
        Boolean, `${runtime}: a held delegation runs after resume`, 120000);
      order.push(markers.findIndex(marker => ask.title.includes(marker)));
      done.add(ask.askId);
      await stopRunning(ask);
    }
    assert.deepEqual([...order].sort(), [0, 1], `each held delegation ran: ${order}`);
    await delay(3000);
    for (const marker of markers) assert.equal(turnsWith(await history(b), marker).length, 1, `${marker} delivered exactly once`);
    results.push({ step: 'delegations during a project pause are held; resume delivers each once', held: markers.length, order });
  }

  // 4. Race: a delegation submit and a project pause issued together. A
  //    submit that reached the runtime before the pause committed may run
  //    (after-turn pause); after the pause returns and that turn is stopped,
  //    no member turn starts until resume, and each ask runs exactly once.
  {
    const rounds = [];
    for (let round = 0; round < 4; round++) {
      await idle(a); await idle(b);
      const marker = `race-${round}-${uuid().slice(0, 8)}`;
      const epoch = (await runControl()).epoch ?? 0;
      const sent = delegate(marker, `race ${round}`);
      await delay([0, 300, 700, 1200][round]);
      const paused = await api(`/v2/projects/${encodeURIComponent(project)}/pause`, { actionId: `pause-${uuid()}`, expectedEpoch: epoch, mode: 'afterTurn' });
      await sent;
      await askFor(marker);
      await delay(2000);
      const early = await askFor(marker);
      const raced = early.execution === 'running' && early.attempt?.turnId;
      if (raced) await stopRunning(early);
      const settled = (await history(b)).length;
      await delay(6000);
      assert.equal((await history(b)).length, settled, `${runtime}: round ${round}: a member turn started after the pause`);
      if (!raced) assert.notEqual((await askFor(marker)).execution, 'running', `${runtime}: round ${round}: admitted while paused`);
      await resume();
      if (!raced) await stopRunning(await running(marker));
      await delay(2000);
      assert.equal(turnsWith(await history(b), marker).length, 1, `${runtime}: race ${round} delivered exactly once`);
      rounds.push({ outcome: raced ? 'submitted-before-pause' : 'held-by-pause', attempts: (await askFor(marker)).attempts, pausedAt: paused.runControl?.requestedAt ?? null });
    }
    results.push({ step: 'delegation submit racing a project pause', rounds });
  }
  // 5. (r267 B3) "全部暂停": the owner pauses the project with scope `all`
  //    (Director included). Delegations from an agent outside the project to
  //    the member AND to the Director, and owner input to the Director, are
  //    held, not run; resume delivers each exactly once.
  if (outside) {
    await idle(a); await idle(b); await idle(outside);
    const before = { a: (await history(a)).length, b: (await history(b)).length };
    const control = await runControl();
    const paused = await api(`/v2/projects/${encodeURIComponent(project)}/pause`, { actionId: `pause-${uuid()}`, expectedEpoch: control.epoch ?? 0, mode: 'afterTurn', scope: 'all' });
    const pausedControl = await runControl();
    assert.equal(pausedControl.scope, 'all', JSON.stringify(pausedControl));
    const toB = `pauseall-b-${uuid().slice(0, 8)}`, toA = `pauseall-a-${uuid().slice(0, 8)}`, ownerA = `pauseall-owner-${uuid().slice(0, 8)}`;
    await send(outside, delegateCall({ target: b.id, text: `deliver ${toB}`, title: `all ${toB}` }));
    await askFor(toB);
    await idle(outside);
    await send(outside, delegateCall({ target: a.id, text: `deliver ${toA}`, title: `all ${toA}` }));
    await askFor(toA);
    let ownerInput;
    try { ownerInput = { accepted: true, response: JSON.stringify(await daemon.send(a, `owner ${ownerA}`)).slice(0, 400) }; }
    catch (error) { ownerInput = { accepted: false, error: String(error.message).slice(0, 400) }; }
    await delay(10000);
    const held = { a: (await history(a)).length, b: (await history(b)).length };
    assert.deepEqual(held, before, `${runtime}: a turn ran on a paused agent under pause-all`);
    const heldAsks = { toB: (await askFor(toB)).execution, toA: (await askFor(toA)).execution };
    assert.ok(Object.values(heldAsks).every(execution => execution !== 'running' && execution !== 'answered'), JSON.stringify(heldAsks));
    await resume();
    for (const [agent, marker] of [[b, toB], [a, toA]]) {
      await poll(() => history(agent), turns => turnsWith(turns, marker).length >= 1 && turns.at(-1).status !== 'inProgress', `${runtime}: ${marker} delivered after resume`, 180000);
    }
    await delay(6000);
    const once = { toB: turnsWith(await history(b), toB).length, toA: turnsWith(await history(a), toA).length, ownerA: turnsWith(await history(a), ownerA).length };
    assert.deepEqual([once.toB, once.toA], [1, 1], JSON.stringify(once));
    assert.ok(once.ownerA <= 1, 'owner input to the paused Director never ran twice');
    results.push({ step: 'pause all (scope all, Director included): deliveries held, then exactly once', pauseScope: pausedControl.scope, pausePhase: pausedControl.phase,
      heldTurnCounts: held, heldAsks, ownerInputWhilePaused: ownerInput, afterResume: once, askExecution: { toB: (await askFor(toB)).execution, toA: (await askFor(toA)).execution } });
  }
  results.push({ ownerInputDeferredRetries: deferred });
  return results;
}
