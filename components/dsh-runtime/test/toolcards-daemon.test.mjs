import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { install, binary, privateDaemon, poll, uuid, offlinePatch } from './private-daemon.mjs';

// Private debug daemon + real DSH + offline scripted model: what a desktop reads
// from cutex's Management history for a DSH agent's shell command and file edit.
// Opt-in only (DSH_SPIKE_INSTALL + DSH_SPIKE_DAEMON).
const plugin = fileURLToPath(new URL('./toolcards-mock-llm.mjs', import.meta.url));

test('Management history shows DSH shell commands and file edits as commandExecution / fileChange', { skip: !install || !binary, timeout: 600000 }, async t => {
  const daemon = await privateDaemon(t, { label: 'toolcards', patch: offlinePatch(plugin), env: { DSH_TOOLCARDS_TEST_KEY: `dummy-toolcards-key-${uuid()}` } });
  const { api, history, send } = daemon;
  const { profile } = await api('/v2/profiles', { name: 'DSH Tool Cards Offline', kind: 'dsh', options: { dsh: { model: 'offline', apiKeyEnv: 'DSH_TOOLCARDS_TEST_KEY' } } });
  const agent = await daemon.createAgent('dsh-cards', profile.id);
  await daemon.online(agent);
  const script = [
    ['write', { file_path: 'cards.txt', content: 'one\ntwo\n' }],
    ['edit', { file_path: 'cards.txt', old_string: 'two', new_string: 'TWO' }],
    ['bash', { command: 'cat cards.txt; exit 5', description: 'Show the file' }],
    ['todo_write', { todos: [{ content: 'Cards', status: 'completed' }] }],
  ];
  await send(agent, `SCRIPT ${JSON.stringify(script)}`);
  const turns = await poll(() => history(agent), list => list.at(-1)?.status === 'completed'
    && list.at(-1).items.some(item => item.type === 'agentMessage' && item.text.startsWith('script done')), 'scripted turn completed');
  const tools = turns.at(-1).items.filter(item => !['userMessage', 'agentMessage', 'reasoning', 'functionCallOutput'].includes(item.type));
  assert.deepEqual(tools.map(item => item.type), ['fileChange', 'fileChange', 'commandExecution'], JSON.stringify(tools).slice(0, 3000));
  const [created, edited, command] = tools;
  assert.equal(created.changes[0].kind.type, 'add');
  assert.equal(created.changes[0].diff, 'one\ntwo\n');
  assert.equal(edited.changes[0].diff, '@@\n one\n-two\n+TWO\n');
  assert.equal(command.command, 'cat cards.txt; exit 5');
  assert.equal(command.aggregatedOutput, 'one\nTWO\n');
  assert.equal(command.exitCode, 5);
  assert.ok(!turns.at(-1).items.some(item => item.type === 'userMessage' && JSON.stringify(item).includes('Current runtime context')));
  // The checklist reaches Management's event stream as Codex's turn/plan/updated.
  const page = await api(`/v2/events?cutexSessionId=${encodeURIComponent(agent.id)}&limit=1000`);
  const found = [];
  const walk = value => {
    if (Array.isArray(value)) value.forEach(walk);
    else if (value && typeof value === 'object') { if (value.method === 'turn/plan/updated') found.push(value.params); Object.values(value).forEach(walk); }
  };
  walk(page);
  assert.deepEqual(found.at(-1)?.plan, [{ step: 'Cards', status: 'completed' }], JSON.stringify(page).slice(0, 2000));
});
