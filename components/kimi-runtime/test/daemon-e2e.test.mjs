import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, readFileSync, readdirSync, existsSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { binary, privateDaemon, poll, userTexts, agentTexts, uuid } from './private-daemon.mjs';
import { startMock } from './mock-llm.mjs';

// r266 Kimi Code acceptance on a private debug daemon (Linux), offline: the
// kimi-runtime component installed with `cutex component install
// --from-source`, a Kimi / Moonshot API-key Profile with a fake stored key and
// its kimi variant, the scripted mock provider (no network, no real key).
// Opt-in:
//   KIMI_E2E_DAEMON     debug cutex binary (cutex-mcp beside it)
//   KIMI_E2E_COMPONENT  a build.sh payload (stage dir or extracted archive)
// Covers: two Kimi agents (human path + agent_create), a turn with a tool
// approval, a plan as latestPlan, delegation round trip, agent_tell delivery,
// interrupt, model change, /v2/usage + latestTokenUsage, revert, fork (start
// it), delete, history after stop, the stray-.git refusal, no key leak.
const component = process.env.KIMI_E2E_COMPONENT;
const KEY = `sk-fake-r266-kimi-e2e-${uuid()}`;

function filesContaining(root, secret, found = []) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isSymbolicLink()) continue;
    if (entry.isDirectory()) { if (entry.name !== 'node_modules') filesContaining(path, secret, found); continue; }
    if (entry.isFile() && readFileSync(path).includes(secret)) found.push(path);
  }
  return found;
}

test('r266 Kimi Code: variant agents, approvals, delivery, delegation, structure, usage, no key leak', { skip: !component || !binary, timeout: 30 * 60 * 1000 }, async t => {
  const mock = await startMock();
  t.after(() => mock.close());
  const daemon = await privateDaemon(t, { label: 'kimi', component, baseUrl: `http://127.0.0.1:${mock.port}/v1`, secrets: [KEY] });
  const { api, history, send, idle, state, request } = daemon;
  const responses = [];
  const call = async (...args) => { const value = await api(...args); responses.push(value); return value; };
  const results = [];
  const step = async (name, fn) => {
    const started = Date.now();
    try { const info = await fn(); results.push({ step: name, ok: true, secs: Math.round((Date.now() - started) / 1000), info: info ?? null }); }
    catch (error) { results.push({ step: name, ok: false, secs: Math.round((Date.now() - started) / 1000), info: String(error.message).replaceAll(KEY, '[redacted]').slice(0, 900) }); }
  };
  const app = (agent, method, params) => call(agent.path + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method, params } } });
  const result = response => response.native?.message?.result ?? response.native?.result;
  const cutexOk = async (agent, method, params) => (await request(agent, method, params)).cutex.result;
  const agentFor = (id, facade, name) => {
    const agent = { name, id, facade, path: `/v2/sessions/${encodeURIComponent(id)}` };
    daemon.agents.push(agent);
    return agent;
  };
  const completedAfter = (agent, before, description, timeout = 180000) =>
    poll(() => history(agent), turns => turns.length > before && turns.at(-1)?.status !== 'inProgress', description, timeout);
  let profile, a, b;

  await step('component installed; host advertises kimi-runtime; profile with a stored key + kimi variant', async () => {
    const capabilities = await call('/v2/capabilities');
    assert.ok((capabilities.featureList ?? []).includes('kimi-runtime'), JSON.stringify(capabilities.featureList));
    ({ profile } = await call('/v2/profiles', { name: 'Moonshot E2E', kind: 'apiKey', preset: 'moonshot', baseUrl: 'https://api.moonshot.ai/v1', model: 'kimi-k2.6', apiKey: KEY }));
    const added = await call(`/v2/profiles/${encodeURIComponent(profile.id)}/variants`, { runtime: 'kimi', defaults: { model: 'kimi-k2.6' } });
    const row = added.profile.runtimeCompatibility.find(entry => entry.runtime === 'kimi');
    assert.equal(row.status, 'supported', JSON.stringify(row));
    assert.equal(added.profile.credential.source, 'stored');
    return { variants: added.profile.runtimeVariants.map(variant => variant.id), kimi: row.status };
  });

  await step('a folder under a stray empty .git is refused at start (Kimi would read above it)', async () => {
    const stray = join(daemon.home, 'stray');
    mkdirSync(join(stray, '.git'), { recursive: true });
    mkdirSync(join(stray, 'agent'));
    const created = await call('/v2/human/agents', { requestId: uuid(), name: 'kimi-e2e-stray', cwd: join(stray, 'agent'), profile: profile.id, variantId: 'kimi', start: false });
    const stray_agent = agentFor(created.cutexSessionId, created.session.native.threadId, 'kimi-e2e-stray');
    let refused = null;
    try { await request(stray_agent, 'cutex/runtime/online', { expectedRuntimeGeneration: 0, openVisibleTerminal: false, reason: 'stray' }); }
    catch (error) { refused = error.message; }
    assert.match(refused ?? '', /git init/, refused);
    daemon.agents.splice(daemon.agents.indexOf(stray_agent), 1);
    await cutexOk(stray_agent, 'cutex/session/delete', {});
    return { refused: refused.slice(0, 160) };
  });

  await step('agent A from the kimi variant (human path), online, model/list', async () => {
    const created = await call('/v2/human/agents', { requestId: uuid(), name: 'kimi-e2e-a', cwd: daemon.cwd, profile: profile.id, variantId: 'kimi', start: false });
    a = agentFor(created.cutexSessionId, created.session.native.threadId, 'kimi-e2e-a');
    await request(a, 'cutex/session/defaults/update', { patch: { sandboxMode: 'workspace-write', approvalPolicy: 'on-request' } });
    await daemon.online(a);
    const session = await state(a);
    assert.equal(session.launchBinding?.variantId, 'kimi');
    assert.equal(session.launchBinding?.runtime, 'kimi');
    assert.equal(session.native?.schema?.version, 'kimi-acp-v1');
    assert.equal(session.native?.schema?.capabilities?.features?.billingUsage, true);
    const models = result(await app(a, 'model/list', { limit: 50 }))?.data ?? [];
    assert.ok(models.some(model => model.id === 'kimi-k2.6') && models.some(model => model.id === 'kimi-k2.7-code'), JSON.stringify(models.map(model => model.id)));
    return { schema: session.native.schema.version, models: models.map(model => model.id) };
  });

  await step('A: a command needs the owner\'s approval; accepted, it runs as a commandExecution item', async () => {
    const before = (await history(a)).length;
    await send(a, 'CALL Bash {"command":"echo e2e-approved > approved.txt && echo ran-it"}');
    const pending = await poll(() => state(a), value => value.runtime?.pendingServerRequests?.length === 1, 'approval pending', 120000);
    const ask = pending.runtime.pendingServerRequests[0];
    assert.equal(ask.method, 'item/commandExecution/requestApproval');
    // The owner's approval card (bootstrap's pending server requests) shows the command.
    const boot = await call(a.path + '/bootstrap');
    const card = JSON.stringify(boot.pendingServerRequests ?? boot.collections?.pendingServerRequests ?? boot);
    const shown = /echo e2e-approved/.test(card);
    await daemon.answer(a, ask, 'accept');
    const turns = await completedAfter(a, before, 'approved turn');
    assert.equal(turns.at(-1).status, 'completed');
    assert.equal(readFileSync(join(daemon.cwd, 'approved.txt'), 'utf8').trim(), 'e2e-approved');
    const command = turns.at(-1).items.find(item => item.type === 'commandExecution');
    assert.ok(command && /ran-it/.test(command.aggregatedOutput ?? ''), JSON.stringify(turns.at(-1).items).slice(0, 800));
    assert.ok(shown, `approval card: ${card.slice(0, 1500)}`);
    return { ask: ask.method, command: command.status };
  });

  await step('A: a plan (TodoList) is turn/plan/updated and latestPlan, kept across stop/online; latestTokenUsage', async () => {
    await idle(a);
    const before = (await history(a)).length;
    await send(a, 'TODO');
    await completedAfter(a, before, 'plan turn');
    const steps = plan => (plan?.plan ?? []).map(entry => `${entry.step}:${entry.status}`);
    const first = (await call(a.path + '/history')).cutexState?.latestPlan;
    assert.deepEqual(steps(first), ['first step:completed', 'second step:inProgress'], JSON.stringify(first));
    await daemon.stop(a);
    const offline = (await call(a.path + '/history')).cutexState?.latestPlan;
    await daemon.online(a);
    const boot = await call(a.path + '/bootstrap');
    assert.deepEqual(offline, first);
    assert.deepEqual(boot.cutexState?.latestPlan, first);
    assert.ok(boot.cutexState?.latestTokenUsage, 'latestTokenUsage');
    return { plan: steps(first), usage: boot.cutexState.latestTokenUsage?.tokenUsage?.total ?? boot.cutexState.latestTokenUsage };
  });

  await step('project with A as Director', async () => {
    await call('/v2/agent-management/project-mutations', { schema: 'cutex/human-management-project-mutation/v1', action_id: `kimi-project-${uuid()}`,
      project_id: 'kimie2e', operation: { kind: 'create_project', presentation: { display_name: 'Kimi E2E', badge_label: 'KI', color: 'cyan' }, workspace_root: daemon.cwd, director_cutex_session_id: a.id } });
    await idle(a);
    await daemon.stop(a);
    await daemon.online(a);
  });

  await step('agent_create from A: Kimi agent B through the variant, delegated task, result back to A (no ask for cutex tools)', async () => {
    await idle(a);
    const before = (await history(a)).length;
    const args = { name: 'kimi-e2e-b', profile: profile.name, variant: 'kimi', sandbox: 'workspace-write', approval_policy: 'on-request', task: 'pong-task: answer it' };
    await send(a, `CALL cutex:agent_create ${JSON.stringify(args)}`);
    const sessions = await poll(() => call('/v2/sessions'), value => JSON.stringify(value).includes('kimi-e2e-b'), 'B exists', 180000);
    const row = (sessions.sessions ?? sessions.items ?? []).find(session => JSON.stringify(session).includes('kimi-e2e-b'));
    const id = row?.cutexSessionId ?? row?.id;
    assert.ok(id, `B session id: ${JSON.stringify(row)?.slice(0, 400)}`);
    const bState = await call(`/v2/sessions/${encodeURIComponent(id)}`);
    b = agentFor(id, bState.native?.threadId, 'kimi-e2e-b');
    assert.equal(bState.launchBinding?.runtime, 'kimi');
    const bTurns = await poll(() => history(b), turns => turns.some(turn => turn.status === 'completed' && agentTexts([turn]).some(text => /pong-task/.test(text))), 'B answered the delegated task', 180000);
    const back = await poll(() => history(a), turns => turns.slice(before).some(turn => turn.items.some(item => item.type === 'functionCallOutput' && item.name === 'external_event' && /pong-task/.test(item.output))),
      'delegation result back in A', 180000);
    assert.equal((await state(a)).runtime?.pendingServerRequests?.length ?? 0, 0, 'cutex MCP tools never ask');
    return { b: bState.launchBinding?.variantId, bReply: agentTexts(bTurns).find(text => /pong-task/.test(text)), aExternal: back.flatMap(turn => turn.items).filter(item => item.name === 'external_event').length };
  });

  await step('agent_tell delivery A -> B (wake)', async () => {
    await idle(a); await idle(b);
    await send(a, `CALL cutex:agent_tell ${JSON.stringify({ to: b.id, text: 'ping from A', wake: true })}`);
    const told = await poll(() => history(b), turns => userTexts(turns).some(text => text.includes('ping from A')) && turns.at(-1)?.status === 'completed', 'agent_tell answered by B', 180000);
    return { bReply: agentTexts([told.at(-1)]).join(' ').slice(0, 120) };
  });

  await step('interrupt a long B answer', async () => {
    await idle(b);
    await send(b, 'SLOW 300');
    const active = await poll(() => state(b), value => value.runtime?.activeTurnId, 'B turn active', 60000);
    const turnId = active.runtime.activeTurnId;
    await new Promise(done => setTimeout(done, 1500));
    await app(b, 'turn/interrupt', { threadId: b.facade, turnId });
    const turns = await poll(() => history(b), value => value.some(turn => turn.id === turnId && turn.status !== 'inProgress'), 'interrupted', 60000);
    assert.equal(turns.find(turn => turn.id === turnId).status, 'interrupted');
    return { status: 'interrupted' };
  });

  await step('live model change on B applies to the next request', async () => {
    await idle(b);
    const updated = result(await app(b, 'thread/settings/update', { threadId: b.facade, model: 'kimi-k2.7-code' }));
    assert.equal(updated?.threadSettings?.model, 'kimi-k2.7-code', JSON.stringify(updated));
    const before = (await history(b)).length;
    await send(b, 'MODEL?');
    const turns = await completedAfter(b, before, 'turn on the new model');
    assert.match(agentTexts([turns.at(-1)]).join(' '), /model=kimi-k2\.7-code/);
    return { reply: agentTexts([turns.at(-1)]).join(' ') };
  });

  await step('/v2/usage counts the Kimi requests per model and Profile', async () => {
    const usage = await poll(() => call('/v2/usage?groupBy=model'), value => JSON.stringify(value).includes('kimi-k2.7-code') && JSON.stringify(value).includes('kimi-k2.6'), 'usage by model', 60000);
    const byProfile = await call('/v2/usage?groupBy=profile');
    const row = (byProfile.rows ?? []).find(value => value.key?.profileId === profile.id);
    assert.ok(row?.requests > 0, `usage by profile: ${JSON.stringify(byProfile.rows)?.slice(0, 400)}`);
    return { models: (usage.rows ?? []).map(value => [value.key?.model, value.requests, value.input, value.output]), profileRequests: row.requests };
  });

  await step('revert B\'s last turn (Kimi\'s own context is cut)', async () => {
    await idle(b);
    const turns = await history(b);
    const last = turns.at(-1).id;
    const reverted = await cutexOk(b, 'cutex/session/history/revert', { beforeTurnId: last });
    assert.deepEqual(reverted.removedTurnIds, [last]);
    await poll(() => state(b), value => value.runtime?.appServerConnected, 'B back online', 120000);
    const after = await poll(() => history(b), value => value.length === turns.length - 1, 'B history without the reverted turn', 60000);
    const before = after.length;
    await send(b, 'CONTEXT?');
    const next = await completedAfter(b, before, 'context turn');
    const reply = agentTexts([next.at(-1)]).join(' ');
    assert.doesNotMatch(reply, /MODEL\?/, 'the reverted prompt is gone from Kimi\'s context');
    return { removed: reverted.removedTurnIds.length, turns: after.length, context: reply.slice(0, 160) };
  });

  let fork;
  await step('fork A after its first turn, start the fork (stored key through the inherited binding)', async () => {
    await idle(a);
    const turns = await history(a);
    const forked = await cutexOk(a, 'cutex/session/fork', { afterTurnId: turns[0].id, name: 'kimi-e2e-a-fork' });
    fork = agentFor(forked.newCutexSessionId, null, 'kimi-e2e-a-fork');
    const offline = await call(fork.path + '/history');
    assert.equal((offline.native?.result?.thread?.turns ?? []).length, 1);
    const forkState = await state(fork);
    assert.deepEqual([forkState.launchBinding?.variantId, forkState.launchBinding?.credential?.source], ['kimi', 'stored']);
    fork.facade = forkState.native?.threadId;
    await daemon.online(fork);
    fork.facade = (await state(fork)).native?.threadId ?? fork.facade;
    await send(fork, 'CONTEXT?');
    const after = await poll(() => history(fork), value => value.length === 2 && value.at(-1)?.status === 'completed', 'fork answered', 120000);
    const reply = agentTexts([after.at(-1)]).join(' ');
    assert.match(reply, /user turns: 2/, reply);
    await daemon.stop(fork);
    return { forkedThrough: forked.forkedThroughTurnId === turns[0].id, reply: reply.slice(0, 160) };
  });

  await step('delete the fork', async () => {
    const deleted = await cutexOk(fork, 'cutex/session/delete', {});
    assert.equal(deleted.nativeDelete, 'dshState');
    daemon.agents.splice(daemon.agents.indexOf(fork), 1);
    assert.ok(!existsSync(join(daemon.deploymentDir, fork.id)), 'the fork\'s state moved to recovery');
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
    return { a: live.a.length, b: live.b.length };
  });

  await step('instructions reach Kimi once; the key appears nowhere but its own file', async () => {
    const files = filesContaining(daemon.home, KEY).map(path => path.slice(daemon.home.length));
    const leaked = files.filter(path => !/\/(profile-keys|profiles\/[^/]+)\/(.*\.json|auth\.json)$/.test(path));
    assert.deepEqual(leaked, []);
    assert.ok(!daemon.logs.join('').includes(KEY), 'daemon logs');
    assert.ok(!JSON.stringify(responses).includes(KEY), 'API responses');
    const system = mock.requests.at(-1)?.body?.messages?.filter(message => message.role === 'system').map(message => JSON.stringify(message.content)).join('\n') ?? '';
    assert.equal(system.split('# File references').length - 1, 1, 'cutex instructions exactly once');
    const completions = mock.requests.filter(entry => entry.method === 'POST');
    assert.ok(completions.length > 0 && completions.every(entry => entry.auth === 'present'), 'every provider request carried the key');
    return { keyFiles: files, providerRequests: mock.requests.length };
  });

  console.log(`KIMI E2E: ${JSON.stringify(results)}`);
  assert.deepEqual(results.filter(entry => !entry.ok), []);
  daemon.markPassed();
});

// The subscription route: a Kimi account logged in by the REAL pinned `kimi
// login` (located through the component's CLI hook) against K2's fake OAuth +
// provider server; a Kimi-kind Profile; an agent whose home shares the
// account's credentials through links; a turn on the account's catalog model.
test('r266 Kimi Code: a Kimi account agent (device-code login, shared credentials, account catalog)', { skip: !component || !binary, timeout: 15 * 60 * 1000 }, async t => {
  const { spawn } = await import('node:child_process');
  const { readlinkSync, lstatSync } = await import('node:fs');
  const fakeScript = new URL('../../../tests/fixtures/fake-kimi-oauth.mjs', import.meta.url);
  const fake = spawn(process.execPath, [fileURLToPath(fakeScript), '--port', '0', '--pending', '1'], { stdio: ['pipe', 'pipe', 'pipe'] });
  t.after(async () => { if (fake.exitCode === null) { fake.stdin.end(); fake.kill('SIGKILL'); } });
  const port = await new Promise((done, fail) => {
    let out = '';
    fake.stdout.on('data', chunk => { out += chunk; const line = out.split('\n').find(text => text.startsWith('{')); if (line) done(JSON.parse(line).port); });
    fake.once('exit', code => fail(new Error(`fake OAuth exited ${code}`)));
  });
  const base = `http://127.0.0.1:${port}`;
  const daemon = await privateDaemon(t, { label: 'kacct', component, baseUrl: `${base}/coding/v1`, env: { CUTEX_KIMI_TEST_OAUTH_HOST: base, CUTEX_KIMI_TEST_KEEPER_TICK_MS: '500' } });
  const { api, history, send, state } = daemon;
  const account = await api('/v2/kimi/accounts', { label: 'E2E', region: 'global' });
  const login = await api(`/v2/kimi/accounts/${account.id}/login`, {});
  assert.equal(login.userCode, 'FAKE-1234');
  const done = await poll(() => api(`/v2/kimi/accounts/${account.id}/login/${login.loginId}`), value => !['pending', 'starting'].includes(value.state), 'login finished', 120000);
  assert.equal(done.state, 'succeeded', JSON.stringify(done));
  const { profile } = await api('/v2/profiles', { name: 'Kimi Sub', kind: 'kimi', kimiAccount: account.id });
  const variant = profile.runtimeVariants.find(entry => entry.id === 'kimi');
  assert.equal(profile.runtimeCompatibility.find(row => row.runtime === 'kimi').status, 'supported');
  assert.ok(variant.models.some(model => model.id === 'kimi-code/kimi-for-coding'), JSON.stringify(variant.models));
  const created = await api('/v2/human/agents', { requestId: uuid(), name: 'kimi-acct-a', cwd: daemon.cwd, profile: profile.id, variantId: 'kimi', start: false });
  const agent = { name: 'kimi-acct-a', id: created.cutexSessionId, facade: created.session.native.threadId, path: `/v2/sessions/${encodeURIComponent(created.cutexSessionId)}` };
  daemon.agents.push(agent);
  // Never asks (Kimi mode auto): the canary turns below read the credentials without an owner prompt.
  await daemon.request(agent, 'cutex/session/defaults/update', { patch: { sandboxMode: 'danger-full-access', approvalPolicy: 'never' } });
  await daemon.online(agent);
  const session = await state(agent);
  assert.deepEqual([session.launchBinding?.credential?.source, session.launchBinding?.credential?.kimiAccountId], ['kimiAccount', account.id]);
  await send(agent, 'hello account');
  const turns = await poll(() => history(agent), value => value.at(-1)?.status === 'completed', 'account turn', 120000);
  assert.match(agentTexts(turns).join(' '), /FAKE-KIMI-REPLY/);
  // The agent's private home links the account's credentials (one token file, one refresh lock).
  const kimiHome = join(daemon.deploymentDir, agent.id, 'home', 'kimi');
  const accountHome = join(daemon.home, '.cutex', 'kimi-accounts', account.id);
  for (const name of ['credentials', 'oauth']) {
    assert.ok(lstatSync(join(kimiHome, name)).isSymbolicLink(), `${name} is a link`);
    assert.equal(readlinkSync(join(kimiHome, name)), join(accountHome, name));
  }
  const config = readFileSync(join(kimiHome, 'config.toml'), 'utf8');
  assert.match(config, /managed:kimi-code/);
  assert.doesNotMatch(config, /fake-access|fake-refresh/);
  const leaked = filesContaining(daemon.home, 'fake-refresh-').map(path => path.slice(daemon.home.length)).filter(path => !path.includes('/kimi-accounts/'));
  assert.deepEqual(leaked, [], 'tokens stay in the account home');

  // r266 B1 (host-a QA2 canary): Kimi's own Read tool and a shell command
  // print the linked credentials file. Kimi (the model) sees the tokens, as
  // with the official CLI; nothing cutex records or serves holds them.
  const stats = async () => (await fetch(`${base}/stats`)).json();
  const slotFile = readdirSync(join(accountHome, 'credentials')).find(name => name.endsWith('.json'));
  const credentialFile = join(kimiHome, 'credentials', slotFile);
  const tokensOf = text => { const value = JSON.parse(text); return [value.access_token, value.refresh_token]; };
  const kimiOwn = join(daemon.deploymentDir, agent.id, 'home');
  const records = async (secrets, label) => {
    const found = [];
    const look = (where, text) => { for (const secret of secrets) if (text.includes(secret)) found.push(where); };
    look('history', JSON.stringify(await api(`${agent.path}/history`)));
    look('state', JSON.stringify(await state(agent)));
    look('bootstrap', JSON.stringify(await api(`${agent.path}/bootstrap`)));
    look('daemon logs', daemon.logs.join(''));
    // Every file in the private home but the account home and Kimi's own home (its session logs keep raw tool results).
    for (const secret of secrets) for (const path of filesContaining(daemon.home, secret)) if (!path.includes('/kimi-accounts/') && !path.startsWith(kimiOwn + '/')) found.push(path.slice(daemon.home.length));
    return { label, found };
  };
  const canary = async label => {
    const before = (await history(agent)).length;
    await send(agent, `CALL Read {"path":${JSON.stringify(credentialFile)}}`);
    const read = await poll(() => history(agent), value => value.length > before && value.at(-1)?.status !== 'inProgress', `${label} read turn`, 120000);
    await send(agent, `CALL Bash {"command":"cat '${credentialFile}'"}`);
    const turns = await poll(() => history(agent), value => value.length > before + 1 && value.at(-1)?.status !== 'inProgress', `${label} cat turn`, 120000);
    return [read.at(-1), turns.at(-1)];
  };
  const gen1 = tokensOf(readFileSync(credentialFile, 'utf8'));
  const first = await canary('generation 1');
  assert.ok(first.every(turn => turn.status === 'completed'), JSON.stringify(first).slice(0, 2000));
  assert.ok(JSON.stringify(first).includes('[REDACTED]'), `the tools printed the token (redacted): ${JSON.stringify(first).slice(0, 2000)}`);
  assert.deepEqual(await records(gen1, 'generation 1'), { label: 'generation 1', found: [] });
  // Generation 2: the token nears expiry; cutex's keeper refreshes it (once, no race), Kimi uses the new one.
  const before = await stats();
  const current = JSON.parse(readFileSync(join(accountHome, 'credentials', slotFile), 'utf8'));
  writeFileSync(join(accountHome, 'credentials', slotFile), JSON.stringify({ ...current, expires_at: Math.floor(Date.now() / 1000) + 30 }));
  await poll(() => readFileSync(credentialFile, 'utf8'), text => !tokensOf(text).includes(gen1[1]), 'keeper refreshed the account token', 60000);
  const gen2 = tokensOf(readFileSync(credentialFile, 'utf8'));
  const second = await canary('generation 2');
  assert.ok(second.every(turn => turn.status === 'completed'), JSON.stringify(second).slice(0, 2000));
  const after = await stats();
  assert.deepEqual([after.refreshes - before.refreshes, after.rejectedRefreshes], [1, 0], `one refresh, none rejected: ${JSON.stringify(after)}`);
  assert.deepEqual(await records([...gen1, ...gen2], 'generation 2'), { label: 'generation 2', found: [] });
  // The account's redaction ledger: digests of both generations, never a value.
  const ledger = readFileSync(join(daemon.home, '.cutex', 'runtime', 'kimi', 'redaction', `${account.id}.json`), 'utf8');
  assert.ok([...gen1, ...gen2].every(value => !ledger.includes(value)) && JSON.parse(ledger).length >= 4, ledger);
  await daemon.stop(agent);
  // Offline history (the stopped agent's reader process) and the record files after stop.
  assert.deepEqual(await records([...gen1, ...gen2], 'offline'), { label: 'offline', found: [] });
  daemon.markPassed();
});
