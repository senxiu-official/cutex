import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { createServer } from 'node:net';
import { randomUUID } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, isAbsolute, join, relative } from 'node:path';

const binary = process.env.DSH_SPIKE_DAEMON;
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function unusedPort() {
  const server = createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  assert.ok(![24270, 24260, 25370, 25360].includes(port));
  return port;
}
function run(program, args, env, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(program, args, { env, cwd, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    const timer = setTimeout(() => child.kill('SIGKILL'), 60_000);
    child.stdout.on('data', chunk => stdout += chunk);
    child.stderr.on('data', chunk => stderr += chunk);
    child.once('error', error => { clearTimeout(timer); reject(error); });
    child.once('exit', code => { clearTimeout(timer); code === 0 ? resolve(JSON.parse(stdout)) : reject(new Error(`fixture CLI failed ${code}: ${stderr}`)); });
  });
}

test('private Management capabilities follow actual source install and removal without starting a native runtime', {
  skip: !binary || process.platform !== 'win32', timeout: 120_000,
}, async t => {
  assert.ok(isAbsolute(binary));
  const original = realpathSync(binary);
  assert.match(original.replaceAll('\\', '/'), /\/target\/debug\/cutex\.exe$/i);
  const parent = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(parent, { recursive: true });
  const home = mkdtempSync(join(parent, 'optional-'));
  const identity = spawnSync('whoami.exe', ['/user', '/fo', 'csv', '/nh'], { encoding: 'utf8', windowsHide: true });
  const sid = identity.stdout.match(/S-1-5-21-[0-9-]+/)?.[0];
  assert.ok(sid);
  assert.equal(spawnSync('icacls.exe', [home, '/inheritance:r', '/grant:r', `*${sid}:(OI)(CI)F`, '*S-1-5-18:(OI)(CI)F'], { encoding: 'utf8', windowsHide: true }).status, 0);
  writeFileSync(join(home, '.cutex-test-private-home'), 'DSH optional component HTTP fixture\n');
  const config = join(home, '.cutex'), base = join(home, 'install-base'), source = join(home, 'payload');
  for (const path of [config, base, source]) mkdirSync(path);
  const daemon = join(home, 'cutex.exe');
  copyFileSync(original, daemon);
  const port = await unusedPort();
  const busPort = await unusedPort();
  writeFileSync(join(config, 'config.json'), JSON.stringify({ agent_bus_enabled: true, agent_bus_port: busPort,
    agent_bus_token: randomUUID(), management_port: port }));
  writeFileSync(join(base, 'install.json'), JSON.stringify({ schema: 'cutex/install/v1', os: 'windows', serviceManager: 'fixture-no-services', channel: 'main', activeRelease: 'optional-fixture', createdAt: new Date().toISOString() }));
  // Fixture package files are never executed. This tests the real component
  // lifecycle and Management boundary, not the DSH provider or native sandbox.
  const write = (name, value) => { const file = join(source, name); mkdirSync(dirname(file), { recursive: true }); writeFileSync(file, value); };
  write('component.json', JSON.stringify({ schema: 'cutex/component/v1', name: 'dsh-runtime', version: 'dsh-r2', platforms: ['windows-x64', 'linux-x64'] }));
  for (const name of ['package.json', 'package-lock.json', 'runtime/package.json', 'runtime/package-lock.json']) write(name, '{}');
  for (const name of ['server','history-reader','runtime','rpc','store','reconcile','nativeProjection']) write(`src/${name}.mjs`, "throw new Error('fixture adapter must not run');\n");
  for (const [directory, name, version, entry] of [
    ['runtime/node_modules/@deepseek-ai/dsh', '@deepseek-ai/dsh', '0.2.1-rc.3', 'lib/bin.js'],
    ['runtime/node_modules/@deepseek-ai/dsh-session-format-catalog', '@deepseek-ai/dsh-session-format-catalog', '0.2.1-rc.3', 'lib/index.js'],
    ['runtime/node_modules/ws', 'ws', '8.22.0', 'index.js'],
  ]) { write(`${directory}/package.json`, JSON.stringify({ name, version })); write(`${directory}/${entry}`, "throw new Error('fixture package must not run');\n"); }
  const env = Object.fromEntries(['PATH','Path','SystemRoot','WINDIR','COMSPEC','PATHEXT'].filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, TEMP: home, TMP: home, TMPDIR: home,
    APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'), CUTEX_TEST_PRIVATE_HOME: home,
    CUTEX_TEST_FORBID_REAL_HOME: '1', CUTEX_INSTALL_BASE: base, CUTEX_DSH_NODE: process.execPath });
  const token = randomUUID(), logs = [];
  // Own the Bus explicitly too: component CLI event publication otherwise
  // autostarts it, leaving an untracked child holding the fixture binary/pipes.
  const children = [];
  const start = args => {
    const child = spawn(daemon, args, { env, cwd: home, windowsHide: true, stdio: ['ignore','pipe','pipe'] });
    child.stdout.on('data', chunk => logs.push(String(chunk)));
    child.stderr.on('data', chunk => logs.push(String(chunk)));
    children.push(child);
    return child;
  };
  start(['agent','serve','--port',String(busPort)]);
  start(['management','serve','--port',String(port),'--token',token]);
  let passed = false;
  t.after(async () => {
    for (const child of children.reverse()) {
      if (child.exitCode === null && child.signalCode === null) { const exited = once(child,'exit'); child.kill('SIGKILL'); await exited; }
    }
    if (!passed) { console.error(logs.join('').replaceAll(token,'[fixture-token]').slice(-4000)); return; }
    const rel = relative(parent, home);
    assert.ok(rel && !rel.startsWith('..') && !isAbsolute(rel) && existsSync(join(home,'.cutex-test-private-home')));
    rmSync(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  });
  const capabilities = async () => {
    const reply = await fetch(`http://127.0.0.1:${port}/v2/capabilities`, { headers: { Authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(5000) });
    assert.equal(reply.status, 200); return reply.json();
  };
  let initial;
  for (let attempt = 0; attempt < 100; attempt++) { try { initial = await capabilities(); break; } catch { await pause(100); } }
  assert.ok(initial, 'private Management did not start');
  const hasDsh = caps => caps.featureList.includes('dsh-runtime');
  assert.equal(hasDsh(initial), false);
  assert.ok(initial.featureList.includes('components'));
  const installed = await run(daemon, ['component','install','dsh-runtime','--from-source',source], env, home);
  assert.equal(installed.result.select.selected, true);
  assert.match(installed.component.selected, /^dsh-r2-source-[a-f0-9]{12}$/);
  const afterInstall = await capabilities();
  assert.equal(hasDsh(afterInstall), true);
  assert.equal(afterInstall.features.dshRuntime, true);
  const versionDir = join(base,'components/dsh-runtime',installed.component.selected);
  await run(daemon, ['component','remove','dsh-runtime'], env, home);
  const afterRemove = await capabilities();
  assert.equal(hasDsh(afterRemove), false);
  assert.equal(afterRemove.features.dshRuntime, undefined);
  assert.ok(existsSync(versionDir), 'remove deleted the retained payload');
  assert.equal(existsSync(join(config,'runtime/dsh/deployment.json')), false);
  assert.equal(readFileSync(join(source,'src/server.mjs'),'utf8').includes('must not run'), true);
  passed = true;
});
