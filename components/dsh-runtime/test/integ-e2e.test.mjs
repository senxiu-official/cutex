import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, readFileSync, readdirSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { binary, privateDaemon, poll, userTexts, agentTexts, uuid } from './private-daemon.mjs';

// r265 integration acceptance (real DeepSeek, small) on a private debug daemon
// with the build.sh component installed by `cutex component install`.
// Opt-in:
//   DSH_E2E_COMPONENT  extracted build.sh archive
//   DSH_E2E_KEY_FILE   a DeepSeek TEST key file; stored only as the private
//                      profile's write-only key, never printed
//   DSH_PRODUCT_DAEMON debug cutex binary
// Covers: DeepSeek API-key profile with a stored key + DSH variant; a DSH agent
// from the variant; a turn with an approval; a command, a file change and a
// checklist as rich items, the checklist as latestPlan after a reconnect; agent_create of a second DSH agent
// through the variant with a delegated task whose result returns; agent_tell
// delivery; interrupt; live model change; revert (no preview of the removed turn); fork, then start the fork
// (stored key through the inherited binding); delete; no DSH-injected user
// messages in the history; history after the adapters stop; /v2/usage (also by
// Profile); key version 1; billingUsage; the key appears nowhere.
const component = process.env.DSH_E2E_COMPONENT, keyFile = process.env.DSH_E2E_KEY_FILE;

function filesContaining(root, secret, found = []) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isSymbolicLink()) continue;
    if (entry.isDirectory()) { if (entry.name !== 'node_modules') filesContaining(path, secret, found); continue; }
    if (!entry.isFile()) continue;
    // A live SQLite WAL index (-shm/-wal) may vanish between listing and reading.
    let bytes;
    try { bytes = readFileSync(path); } catch (error) { if (error.code === 'ENOENT') continue; throw error; }
    if (bytes.includes(secret)) found.push(path);
  }
  return found;
}

test('r265 integration: variant DSH agents, agent_create, delivery, structure, usage, no key leak', { skip: !component || !keyFile || !binary, timeout: 45 * 60 * 1000 }, async t => {
  const key = readFileSync(keyFile, 'utf8').trim();
  assert.ok(key.length > 10, 'test key file is empty');
  const daemon = await privateDaemon(t, { label: 'integ', component, secrets: [key] });
  const { api, history, send, idle, state, request } = daemon;
  const responses = [];
  const call = async (...args) => { const value = await api(...args); responses.push(value); return value; };
  const results = [];
  const step = async (name, fn) => {
    const started = Date.now();
    try { const info = await fn(); results.push({ step: name, ok: true, secs: Math.round((Date.now() - started) / 1000), info: info ?? null }); }
    catch (error) { results.push({ step: name, ok: false, secs: Math.round((Date.now() - started) / 1000), info: String(error.message).replaceAll(key, '[redacted]').slice(0, 600) }); }
  };
  const app = (agent, method, params) => call(agent.path + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method, params } } });
  const cutexOk = async (agent, method, params) => (await request(agent, method, params)).cutex.result;
  const agentFor = (id, facade, name) => {
    const agent = { name, id, facade, path: `/v2/sessions/${encodeURIComponent(id)}` };
    daemon.agents.push(agent);
    return agent;
  };
  let profile, a, b;

  await step('profile: DeepSeek API key (stored) + DSH variant; presets/variants list the DSH catalog', async () => {
    const presets = await call('/v2/profile-presets');
    const deepseek = presets.presets.find(preset => preset.id === 'deepseek');
    assert.equal(deepseek.runtimeCompatibility.find(row => row.runtime === 'dsh').status, 'supported');
    ({ profile } = await call('/v2/profiles', { name: 'DeepSeek Integ', kind: 'apiKey', preset: 'deepseek', apiKey: key }));
    const added = await call(`/v2/profiles/${encodeURIComponent(profile.id)}/variants`, { runtime: 'dsh', defaults: { model: 'deepseek-flash', reasoningEffort: 'low' } });
    const dsh = added.profile.runtimeVariants.find(variant => variant.id === 'dsh');
    assert.deepEqual(dsh.models.map(model => model.id), ['deepseek-flash', 'deepseek-v4-pro']);
    assert.equal(added.profile.credential.source, 'stored');
    // C-6: the key written at creation is version 1.
    assert.equal(added.profile.credential.version, 1);
    return { credential: added.profile.credential.source, version: added.profile.credential.version, variants: added.profile.runtimeVariants.map(variant => variant.id) };
  });

  await step('agent A from the DSH variant (human path), online, model/list', async () => {
    const created = await call('/v2/human/agents', { requestId: uuid(), name: 'dsh-integ-a', cwd: daemon.cwd, profile: profile.id, variantId: 'dsh', start: false });
    a = agentFor(created.cutexSessionId, created.session.native.threadId, 'dsh-integ-a');
    await request(a, 'cutex/session/defaults/update', { patch: { sandboxMode: 'workspace-write', approvalPolicy: 'on-request' } });
    await daemon.online(a);
    const session = await state(a);
    assert.equal(session.launchBinding?.variantId, 'dsh');
    assert.equal(session.launchBinding?.credential?.version, 1);
    // C-5: DSH reports token usage.
    assert.equal(session.native?.schema?.capabilities?.features?.billingUsage, true);
    const listed = await app(a, 'model/list', { limit: 50 });
    const models = (listed.native?.message?.result ?? listed.native?.result)?.data ?? [];
    return { binding: { variantId: session.launchBinding.variantId, runtime: session.launchBinding.runtime, credential: session.launchBinding.credential?.source }, models: models.map(model => model.id) };
  });

  await step('A: a tool call that needs approval (write outside the workspace)', async () => {
    const outside = join(daemon.home, 'outside');
    mkdirSync(outside, { recursive: true });
    const target = join(outside, 'approved.txt');
    await send(a, `Use the write tool to create the file ${target} with the content "integ ok". It is outside the workspace, so set sandbox_permissions to "danger-full-access" and give a one-line justification. Do nothing else and reply "done".`);
    const pending = await poll(() => state(a), value => value.runtime?.pendingServerRequests?.length === 1, 'approval pending', 240000);
    await daemon.answer(a, pending.runtime.pendingServerRequests[0], 'accept');
    await poll(() => history(a), turns => turns.at(-1)?.status === 'completed', 'approved turn completed', 240000);
    assert.equal(readFileSync(target, 'utf8').trim(), 'integ ok');
    return { method: pending.runtime.pendingServerRequests[0].method };
  });

  await step('A: a command, a file change and a checklist as rich items; the plan survives a re-read', async () => {
    await idle(a);
    const before = (await history(a)).length;
    await send(a, 'Do exactly these steps with your tools, one at a time: 1) call todo_write with two todos: "Run the command" (in_progress) and "Write the note" (pending); '
      + '2) run the shell command `echo integ-cmd` with the bash tool; 3) create the file note.txt in the workspace with the content "integ note" using the write tool; '
      + '4) call todo_write again with both todos completed. Then reply "done".');
    // Accept any approval DSH asks for (none is expected inside the workspace).
    const turns = await poll(async () => {
      const current = await state(a);
      for (const pending of current.runtime?.pendingServerRequests ?? []) await daemon.answer(a, pending, 'accept');
      return history(a);
    }, value => value.length > before && value.at(-1)?.status === 'completed', 'tool turn completed', 300000);
    const items = turns.slice(before).flatMap(turn => turn.items);
    const command = items.find(item => item.type === 'commandExecution');
    const change = items.find(item => item.type === 'fileChange');
    assert.ok(command, `commandExecution item: ${JSON.stringify(items.map(item => item.type))}`);
    assert.ok(change, `fileChange item: ${JSON.stringify(items.map(item => item.type))}`);
    assert.match(command.command ?? '', /echo integ-cmd/);
    assert.match(command.aggregatedOutput ?? '', /integ-cmd/);
    assert.ok(change.changes?.some(entry => entry.path?.endsWith('note.txt')), JSON.stringify(change).slice(0, 400));
    assert.equal(readFileSync(join(daemon.cwd, 'note.txt'), 'utf8').trim(), 'integ note');
    // The checklist (todo_write) is bootstrap / history cutexState.latestPlan,
    // kept by the daemon: also after the runtime stops and comes back (reconnect).
    const steps = plan => (plan?.plan ?? []).map(entry => entry.step);
    const first = (await call(a.path + '/history')).cutexState?.latestPlan;
    assert.ok(steps(first).length >= 2 && steps(first).some(step => /command/i.test(step)), JSON.stringify(first));
    assert.equal(first.threadId, a.facade);
    await daemon.stop(a);
    const offline = (await call(a.path + '/history')).cutexState?.latestPlan;
    await daemon.online(a);
    const bootstrap = (await call(a.path + '/bootstrap')).cutexState?.latestPlan;
    assert.deepEqual(offline, first);
    assert.deepEqual(bootstrap, first);
    return { command: command.status, fileChange: change.status, plan: first.plan.map(entry => entry.status), usage: Boolean((await call(a.path + '/bootstrap')).cutexState?.latestTokenUsage) };
  });

  await step('project with A as Director', async () => {
    const created = await call('/v2/agent-management/project-mutations', { schema: 'cutex/human-management-project-mutation/v1', action_id: `integ-project-${uuid()}`,
      project_id: 'integ', operation: { kind: 'create_project', presentation: { display_name: 'Integ', badge_label: 'IN', color: 'cyan' }, workspace_root: daemon.cwd, director_cutex_session_id: a.id } });
    // The Director's tools (agent_create) are listed when its runtime starts.
    await idle(a);
    await daemon.stop(a);
    await daemon.online(a);
    return { ok: Boolean(created) };
  });

  await step('agent_create from A: DSH agent B through the variant, delegated task, result back to A', async () => {
    await idle(a);
    const before = (await history(a)).length;
    await send(a, `Call the cutex tool agent_create with name="dsh-integ-b", profile="${profile.name}", variant="dsh", sandbox="workspace-write", approval_policy="on-request", task="Reply with the single word pong and nothing else." Then reply "created".`);
    const sessions = await poll(() => call('/v2/sessions'), value => JSON.stringify(value).includes('dsh-integ-b'), 'B exists', 300000);
    const row = (sessions.sessions ?? sessions.items ?? []).find(session => JSON.stringify(session).includes('dsh-integ-b'));
    const id = row?.cutexSessionId ?? row?.id;
    assert.ok(id, `B session id: ${JSON.stringify(row)?.slice(0, 400)}`);
    const bState = await call(`/v2/sessions/${encodeURIComponent(id)}`);
    b = agentFor(id, bState.native?.threadId, 'dsh-integ-b');
    assert.equal(bState.runtime?.backend ?? bState.runtimeBackend ?? bState.runtime?.runtimeBackend ?? 'dsh', 'dsh');
    const bTurns = await poll(() => history(b), turns => turns.some(turn => turn.status === 'completed' && agentTexts([turn]).some(text => /pong/i.test(text))), 'B answered the delegated task', 300000);
    const back = await poll(() => history(a), turns => turns.slice(before).some(turn => turn.items.some(item => item.type === 'functionCallOutput' && item.name === 'external_event' && /pong/i.test(item.output))), 'delegation result back in A', 300000);
    return { bBinding: bState.launchBinding?.variantId ?? null, bTurns: bTurns.length, aExternal: back.flatMap(turn => turn.items).filter(item => item.name === 'external_event').map(item => JSON.parse(item.output).type) };
  });

  await step('agent_tell delivery A -> B (wake)', async () => {
    await idle(a); await idle(b);
    await send(a, `Call the cutex tool agent_tell with to="${b.id}", text="ping from A: reply with the word pong", wake=true. Then reply "sent".`);
    const told = await poll(() => history(b), turns => userTexts(turns).some(text => text.includes('ping from A')) && turns.at(-1)?.status === 'completed', 'agent_tell answered by B', 300000);
    return { bReply: agentTexts([told.at(-1)]).join(' ').slice(0, 80) };
  });

  await step('interrupt a long B answer', async () => {
    await idle(b);
    await send(b, 'Write the numbers from 1 to 2000, one per line, with no other text.');
    const active = await poll(() => state(b), value => value.runtime?.activeTurnId, 'B turn active', 120000);
    const turnId = active.runtime.activeTurnId;
    await new Promise(done => setTimeout(done, 3000));
    await app(b, 'turn/interrupt', { threadId: b.facade, turnId });
    const turns = await poll(() => history(b), value => value.some(turn => turn.id === turnId && turn.status !== 'inProgress'), 'interrupted', 120000);
    const status = turns.find(turn => turn.id === turnId).status;
    assert.equal(status, 'interrupted');
    return { status };
  });

  await step('live model change on B (deepseek-v4-pro, low) applies to the next turn', async () => {
    await idle(b);
    const updated = await app(b, 'thread/settings/update', { threadId: b.facade, model: 'deepseek-v4-pro', effort: 'low' });
    const settings = (updated.native?.message?.result ?? updated.native?.result)?.threadSettings;
    assert.equal(settings?.model, 'deepseek-v4-pro');
    const before = (await history(b)).length;
    await send(b, 'Reply with the single word ok.');
    await poll(() => history(b), turns => turns.length > before && turns.at(-1)?.status === 'completed', 'turn on the new model', 240000);
    const read = await app(b, 'thread/read', { threadId: b.facade });
    const thread = (read.native?.message?.result ?? read.native?.result)?.thread;
    assert.equal(thread?.model, 'deepseek-v4-pro');
    return { model: thread.model, effort: thread.reasoningEffort };
  });

  await step('/v2/usage counts the DSH requests per model', async () => {
    const usage = await poll(() => call('/v2/usage'), value => ['deepseek-flash', 'deepseek-v4-pro'].every(model => (value.unpricedModels ?? []).some(row => row.model === model)),
      'usage by model', 120000);
    // C-3: the requests are attributed to the variant agents' Profile.
    const byProfile = await call('/v2/usage?groupBy=profile');
    const row = (byProfile.rows ?? []).find(value => value.key?.profileId === profile.id);
    assert.ok(row?.requests > 0, `usage by profile: ${JSON.stringify(byProfile.rows)?.slice(0, 400)}`);
    return { requests: usage.totals?.requests, input: usage.totals?.input, output: usage.totals?.output, models: usage.unpricedModels.map(row => [row.model, row.requests]),
      byProfile: (byProfile.rows ?? []).map(value => [value.key?.profile ?? null, value.requests]) };
  });

  await step('revert B\'s last turn', async () => {
    await idle(b);
    const turns = await history(b);
    const last = turns.at(-1).id;
    const previewed = (await state(b)).activity?.lastOutput?.displayText ?? null;
    const reverted = await cutexOk(b, 'cutex/session/history/revert', { beforeTurnId: last });
    assert.deepEqual(reverted.removedTurnIds, [last]);
    // C-7: the agent list no longer previews the removed turn's answer.
    const activity = (await state(b)).activity;
    assert.equal(activity?.lastOutput ?? null, null, `lastOutput after revert (was ${previewed}): ${JSON.stringify(activity?.lastOutput)}`);
    await poll(() => state(b), value => value.runtime?.appServerConnected, 'B back online', 180000);
    const after = await poll(() => history(b), value => value.length === turns.length - 1, 'B history without the reverted turn', 120000);
    return { removed: reverted.removedTurns, restarted: reverted.runtimeRestarted, turns: after.length, previewBefore: previewed };
  });

  let fork;
  await step('fork A after its first turn', async () => {
    await idle(a);
    const turns = await history(a);
    const forked = await cutexOk(a, 'cutex/session/fork', { afterTurnId: turns[0].id, name: 'dsh-integ-a-fork' });
    fork = agentFor(forked.newCutexSessionId, null, 'dsh-integ-a-fork');
    const offline = await call(fork.path + '/history');
    const forkTurns = offline.native?.result?.thread?.turns ?? [];
    assert.equal(forkTurns.length, 1);
    return { forkedThrough: forked.forkedThroughTurnId === turns[0].id, forkTurns: forkTurns.length };
  });

  await step('the fork (stored key) has the source binding, starts and answers', async () => {
    assert.ok(fork, 'fork exists');
    const source = (await state(a)).launchBinding;
    const forkState = await state(fork);
    const bound = forkState.launchBinding;
    assert.ok(bound, 'fork launch binding');
    assert.deepEqual([bound.profileId, bound.variantId, bound.runtime, bound.credential?.source, bound.credential?.version],
      [source.profileId, source.variantId, source.runtime, source.credential?.source, 1]);
    fork.facade = forkState.native?.threadId;
    await daemon.online(fork);
    fork.facade = (await state(fork)).native?.threadId ?? fork.facade;
    await send(fork, 'Reply with the single word forked.');
    const turns = await poll(() => history(fork), value => value.length === 2 && value.at(-1)?.status === 'completed', 'fork answered', 240000);
    assert.match(agentTexts([turns.at(-1)]).join(' '), /forked/i);
    await daemon.stop(fork);
    return { binding: [bound.variantId, bound.credential?.source, bound.credential?.version], turns: turns.length };
  });

  await step('DSH-injected context and model-change messages are not owner messages', async () => {
    const texts = [...userTexts(await history(a)), ...userTexts(await history(b))];
    const injected = texts.filter(text => text.startsWith('Current runtime context') || text.startsWith('[model changed'));
    assert.deepEqual(injected, []);
    return { userMessages: texts.length };
  });

  await step('delete the fork', async () => {
    const deleted = await cutexOk(fork, 'cutex/session/delete', {});
    assert.equal(deleted.nativeDelete, 'dshState');
    daemon.agents.splice(daemon.agents.indexOf(fork), 1);
    return { nativeDelete: deleted.nativeDelete };
  });

  await step('history after the adapters stop equals the live history', async () => {
    const shape = turns => turns.map(turn => ({ id: turn.id, status: turn.status, items: turn.items.map(item => item.type) }));
    const live = { a: await history(a), b: await history(b) };
    await daemon.stop(a); await daemon.stop(b);
    const offA = await call(a.path + '/history'), offB = await call(b.path + '/history');
    assert.equal(offA.source, 'offline');
    assert.deepEqual(shape(offA.native.result.thread.turns), shape(live.a));
    assert.deepEqual(shape(offB.native.result.thread.turns), shape(live.b));
    return { a: shape(live.a).map(turn => turn.status), b: shape(live.b).map(turn => turn.status) };
  });

  await step('the key appears nowhere (private home files, daemon logs, responses)', async () => {
    const files = filesContaining(daemon.home, key).map(path => path.slice(daemon.home.length));
    // The stored key's own private file is the only place it may live.
    const leaked = files.filter(path => !/\/(profile-keys|profiles\/[^/]+)\/(.*\.json|auth\.json)$/.test(path));
    assert.deepEqual(leaked, []);
    assert.ok(!daemon.logs.join('').includes(key), 'daemon logs');
    assert.ok(!JSON.stringify(responses).includes(key), 'API responses');
    return { keyFiles: files };
  });

  console.log(`INTEG E2E: ${JSON.stringify(results)}`);
  assert.deepEqual(results.filter(result => !result.ok), []);
  daemon.markPassed();
});
