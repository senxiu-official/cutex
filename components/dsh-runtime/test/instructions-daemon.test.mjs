import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { install, binary, privateDaemon, poll, agentTexts, uuid, offlinePatch } from './private-daemon.mjs';

// Private daemon + real DSH + offline model: a DSH agent's model sees cutex's
// harness, the user AGENTS.md and the file-reference rules once each, and the
// project AGENTS.md exactly once (DSH's own loader; cutex does not repeat it).
const plugin = fileURLToPath(new URL('./delivery-mock-llm.mjs', import.meta.url));

test('DSH agents get cutex instructions like Claude agents, project AGENTS.md once', { skip: !install || !binary, timeout: 600000 }, async t => {
  const daemon = await privateDaemon(t, { label: 'instructions', patch: offlinePatch(plugin), env: { DSH_INSTRUCTIONS_TEST_KEY: `dummy-${uuid()}` } });
  const { api, history, send } = daemon;
  mkdirSync(join(daemon.home, '.cutex', 'codex-home'), { recursive: true });
  const userFile = join(daemon.home, '.cutex', 'codex-home', 'AGENTS.md');
  writeFileSync(userFile, 'USER-AGENTS-MARKER: answer briefly.\n');
  mkdirSync(join(daemon.cwd, '.git'));
  const projectFile = join(daemon.cwd, 'AGENTS.md');
  writeFileSync(projectFile, 'PROJECT-AGENTS-MARKER: run the tests.\n');
  const { profile } = await api('/v2/profiles', { name: 'DSH Instructions Offline', kind: 'dsh', options: { dsh: { model: 'offline', apiKeyEnv: 'DSH_INSTRUCTIONS_TEST_KEY' } } });
  const agent = await daemon.createAgent('dsh-instructed', profile.id);
  const online = await daemon.online(agent);
  assert.deepEqual(online.runtime.instructionSources, [userFile, projectFile]);
  await send(agent, 'SHOW-PROMPT');
  const turns = await poll(() => history(agent), value => value.at(-1)?.status === 'completed' && agentTexts(value).some(text => text.startsWith('prompt ')), 'prompt report');
  const report = JSON.parse(agentTexts(turns).find(text => text.startsWith('prompt ')).slice('prompt '.length));
  assert.deepEqual(report, { cutex: 1, user: 1, project: 1, fileReferences: 1 });
  daemon.markPassed();
});
