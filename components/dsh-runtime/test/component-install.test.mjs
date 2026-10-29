import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { DatabaseSync } from 'node:sqlite';
import { createHash, randomUUID } from 'node:crypto';
import { createReadStream, existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, realpathSync, cpSync, copyFileSync, chmodSync, readdirSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve, relative, isAbsolute, basename } from 'node:path';
import { fileURLToPath } from 'node:url';

// Explicit DEVELOPMENT inputs only. No service install/start, provider requests,
// owner environment, credential discovery, network listeners or Cargo commands.
const install = process.env.DSH_SPIKE_INSTALL;
const binary = process.env.DSH_SPIKE_DAEMON;
const checkout = fileURLToPath(new URL('../../../', import.meta.url));
const adapter = fileURLToPath(new URL('../', import.meta.url));
const pinned = '0.2.1-rc.3';
const readJson = path => JSON.parse(readFileSync(path, 'utf8').replace(/^\uFEFF/, ''));
const hashFile = async path => {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
};
function inside(root, path) {
  const rel = relative(root, path);
  assert.ok(rel && !isAbsolute(rel) && rel !== '..' && !rel.startsWith(`..${process.platform === 'win32' ? '\\' : '/'}`), `path escapes fixture: ${path}`);
}
function run(program, args, { env, cwd, timeout = 180000 } = {}) {
  return new Promise((done, reject) => {
    const child = spawn(program, args, { env, cwd, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '', timedOut = false;
    const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL'); }, timeout);
    child.stdout.on('data', chunk => { stdout = (stdout + chunk).slice(-2 * 1024 * 1024); });
    child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-16000); });
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      if (code !== 0 || timedOut) return reject(new Error(`${basename(program)} ${args.slice(0, 4).join(' ')}: ${timedOut ? 'timed out' : `exit ${code}/${signal}`}\n${stderr}\n${stdout}`));
      done({ stdout, stderr });
    });
  });
}
function fixture(t, label) {
  assert.ok(isAbsolute(install) && isAbsolute(binary), 'opt-in paths must be absolute');
  const daemon = realpathSync(binary);
  assert.match(daemon.replaceAll('\\', '/'), /\/target\/debug\/cutex(?:\.exe)?$/i, 'only an explicitly supplied development binary is accepted');
  assert.equal(readJson(join(install, 'node_modules/@deepseek-ai/dsh/package.json')).version, pinned);
  const parent = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(parent, { recursive: true });
  const reused = label === 'component-install' && Boolean(process.env.DSH_COMPONENT_REUSE_HOME);
  const home = reused ? realpathSync(process.env.DSH_COMPONENT_REUSE_HOME) : mkdtempSync(join(parent, `${label}-`));
  inside(parent, home);
  const marker = join(home, '.cutex-test-private-home');
  if (reused) assert.equal(readFileSync(marker, 'utf8'), 'keyless DSH component CLI acceptance\n');
  else writeFileSync(marker, 'keyless DSH component CLI acceptance\n');
  const env = Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT'].filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'),
    TEMP: home, TMP: home, TMPDIR: home, CUTEX_TEST_PRIVATE_HOME: home, CUTEX_TEST_FORBID_REAL_HOME: '1',
    CUTEX_INSTALL_BASE: join(home, 'install-base'), CUTEX_DSH_NODE: process.execPath, DSH_TELEMETRY_DISABLED: '1' });
  if (process.platform === 'win32') {
    const identity = spawnSync('whoami.exe', ['/user', '/fo', 'csv', '/nh'], { env, encoding: 'utf8', windowsHide: true });
    const sid = identity.stdout?.match(/S-1-5-21-[0-9-]+/)?.[0];
    assert.ok(sid, 'fixture owner SID missing');
    assert.equal(spawnSync('icacls.exe', [home, '/inheritance:r', '/grant:r', `*${sid}:(OI)(CI)F`, '*S-1-5-18:(OI)(CI)F'], { env, encoding: 'utf8', windowsHide: true }).status, 0);
  } else chmodSync(home, 0o700);
  let succeeded = false;
  t.after(() => {
    // Failed acceptance stays available for inspection; only this marked,
    // unique fixture may ever be recursively removed, never a user home/base.
    if (!succeeded || process.env.DSH_COMPONENT_KEEP === '1') { console.log(`component fixture retained: ${home}`); return; }
    inside(parent, home);
    assert.ok(existsSync(marker));
    rmSync(home, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  });
  // Keep the shared build output replaceable throughout this slow acceptance.
  // Every actual CLI command runs this fixture-owned copy, never target/debug.
  const privateBin = join(home, 'private-bin');
  mkdirSync(privateBin, { recursive: true });
  const privateDaemon = join(privateBin, process.platform === 'win32' ? 'cutex.exe' : 'cutex');
  copyFileSync(daemon, privateDaemon);
  if (process.platform !== 'win32') chmodSync(privateDaemon, 0o700);
  return { home, env, daemon: privateDaemon, reused, passed: () => { succeeded = true; } };
}
function sourceTree(home) {
  // Normal checkout invocation needs no extra 700MB copy. An explicitly supplied
  // separate npm prefix gets an isolated checkout-shaped source, never a link.
  const staged = join(checkout, '.dsh-install');
  if (existsSync(staged) && realpathSync(staged) === realpathSync(install)) return checkout;
  const source = join(home, 'source-checkout');
  mkdirSync(join(source, 'components/dsh-runtime'), { recursive: true });
  for (const name of ['component.json', 'package.json', 'package-lock.json', 'src']) {
    cpSync(join(adapter, name), join(source, 'components/dsh-runtime', name), { recursive: true });
  }
  mkdirSync(join(source, '.dsh-install'));
  for (const name of ['package.json', 'package-lock.json', 'node_modules']) {
    cpSync(join(install, name), join(source, '.dsh-install', name), { recursive: true, filter: path => basename(path) !== '.bin' });
  }
  return source;
}

test('private component source install selects real pinned DSH payload; remove/prune retain saved pins and data', {
  skip: !install || !binary, timeout: 30 * 60 * 1000,
}, async t => {
  const f = fixture(t, 'component-install');
  const base = f.env.CUTEX_INSTALL_BASE, config = join(f.home, '.cutex');
  mkdirSync(base, { recursive: true }); mkdirSync(config, { recursive: true });
  const database = join(config, 'cutex-sessions.sqlite3');
  if (f.reused && existsSync(database)) {
    // Only the zero-byte file left by the old fixture's mistaken schema
    // assumption may be removed. Never replace a populated session store.
    assert.equal(statSync(database).size, 0, 'reuse requires a pristine installed fixture');
    rmSync(database);
  }
  writeFileSync(join(base, 'install.json'), JSON.stringify({ schema: 'cutex/install/v1', os: process.platform === 'win32' ? 'windows' : 'linux',
    serviceManager: 'fixture-no-services', channel: 'main', createdAt: new Date().toISOString(), activeRelease: 'dsh-component-fixture' }));
  const commandTimeout = Number(process.env.DSH_COMPONENT_INSTALL_TIMEOUT_MS ?? 20 * 60 * 1000);
  assert.ok(Number.isSafeInteger(commandTimeout) && commandTimeout >= 1000, 'invalid fixture CLI timeout');
  const cli = async args => JSON.parse((await run(f.daemon, ['component', ...args], { env: f.env, cwd: f.home, timeout: commandTimeout })).stdout);
  const initial = await cli(['status', 'dsh-runtime']);
  assert.equal(initial.state, f.reused ? 'ready' : 'missing');
  assert.equal(initial.dsh.node.minimum, '24.9.0');
  let component = initial;
  if (!f.reused) {
    const installed = await cli(['install', 'dsh-runtime', '--from-source', sourceTree(f.home)]);
    assert.equal(installed.result.select.selected, true);
    component = installed.component;
  }
  assert.equal(component.state, 'ready');
  assert.equal(component.dsh.node.ready, true);
  const version = component.selected;
  assert.match(version, /^dsh-r\d+-source-[a-f0-9]{12}$/);
  const versionDir = join(base, 'components/dsh-runtime', version);
  const descriptorPath = join(config, 'runtime/dsh/deployment.json');
  const deployment = readJson(descriptorPath);
  assert.equal(deployment.version, pinned);
  assert.equal(realpathSync(deployment.node), realpathSync(process.execPath));
  assert.equal(realpathSync(deployment.adapter), realpathSync(join(versionDir, 'src/server.mjs')));
  assert.equal(realpathSync(deployment.install_root), realpathSync(join(versionDir, 'runtime')));
  assert.equal(realpathSync(deployment.dsh_cli), realpathSync(join(versionDir, 'runtime/node_modules/@deepseek-ai/dsh/lib/bin.js')));
  assert.equal(readJson(join(deployment.install_root, 'node_modules/@deepseek-ai/dsh/package.json')).version, pinned);
  const receipt = readJson(join(versionDir, 'receipt.json'));
  for (const name of ['src/server.mjs', 'src/history-reader.mjs', 'runtime/node_modules/@deepseek-ai/dsh/lib/bin.js']) {
    assert.equal(await hashFile(join(versionDir, name)), receipt.files[name]);
  }
  assert.equal(await hashFile(deployment.dsh_cli), await hashFile(join(install, 'node_modules/@deepseek-ai/dsh/lib/bin.js')));

  // A saved, unstarted deployment fixture exercises the actual pin reader and
  // prune policy. No DSH/provider/adapter process is created by this test.
  const id = `cutex.${randomUUID()}`, stateDir = join(config, 'runtime/dsh', id);
  mkdirSync(join(stateDir, 'home/dsh'), { recursive: true });
  const sentinel = join(stateDir, 'home/dsh/retained-test-data.txt');
  writeFileSync(sentinel, 'saved-session-data-must-survive-unselect\n');
  const now = new Date().toISOString();
  const record = { cutex_session_id: id, host_id: 'component-fixture', cwd: f.home, created_at: now, updated_at: now,
    runtime_backend: 'dsh', dsh_session: { thread_id: randomUUID(), state_dir: stateDir, node: deployment.node, adapter: deployment.adapter,
      dsh_cli: deployment.dsh_cli, patch: join(stateDir, 'profile.patch.yml'), install_root: deployment.install_root, deployment_version: pinned, spike: false } };
  // A read-only component status does not create an empty session database.
  // Let the daemon's supported legacy importer initialize its own schema,
  // rather than assuming tables exist or reproducing its DDL in this fixture.
  writeFileSync(join(config, 'cutex-sessions.json'), JSON.stringify({ sessions: { [id]: record } }));
  const stored = () => {
    const db = new DatabaseSync(database, { readOnly: true });
    try { return db.prepare('SELECT record_json FROM sessions WHERE key=?').get(id).record_json; } finally { db.close(); }
  };
  const before = await cli(['status', 'dsh-runtime']);
  const saved = stored();
  assert.equal(before.versions.find(v => v.version === version).pinnedBy, 1);
  const dry = await cli(['remove', 'dsh-runtime', '--dry-run']);
  assert.ok(dry.actions.length);
  assert.deepEqual(readJson(descriptorPath), deployment);
  await cli(['remove', 'dsh-runtime']);
  assert.equal(existsSync(descriptorPath), false, 'remove must unselect for new agents');
  assert.equal(stored(), saved, 'remove must not rewrite the saved session deployment');
  assert.equal(readFileSync(sentinel, 'utf8'), 'saved-session-data-must-survive-unselect\n');
  const after = await cli(['status', 'dsh-runtime']);
  assert.equal(after.selected, null);
  assert.equal(after.state, 'missing');
  assert.equal(after.versions.find(v => v.version === version).pinnedBy, 1);
  const pruned = await cli(['prune', 'dsh-runtime']);
  assert.deepEqual(pruned.removed, []);
  assert.ok(existsSync(deployment.dsh_cli));
  assert.equal(stored(), saved);
  assert.ok(existsSync(sentinel));
  console.log(JSON.stringify({ componentInstall: 'passed', sourceInstallThisRun: !f.reused, version, pinnedBy: 1, node: before.dsh.node, retainedData: true, sourceFiles: Object.keys(receipt.files).length }));
  f.passed();
});

test('Windows build.ps1 packages the explicit offline npm tree without deployment', {
  skip: !install || !binary || process.platform !== 'win32' || process.env.DSH_COMPONENT_PACK !== '1', timeout: 20 * 60 * 1000,
}, async t => {
  const f = fixture(t, 'component-pack');
  const source = sourceTree(f.home), output = join(f.home, 'offline-output');
  const version = readJson(join(adapter, 'component.json')).version;
  await run(process.env.DSH_COMPONENT_POWERSHELL ?? 'pwsh.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', join(adapter, 'build.ps1'),
    '-Platform', 'windows-x64', '-Version', version, '-OutDir', output, '-SourceTree', source], { env: f.env, cwd: f.home, timeout: 18 * 60 * 1000 });
  const archive = join(output, `component-dsh-runtime-${version}-windows-x64.tar.gz`);
  const info = readJson(join(output, 'BUILD-INFO-component-dsh-runtime-windows-x64.json'));
  assert.equal(info.dshVersion, pinned); assert.equal(info.nodeMinimum, '24.9.0');
  assert.equal(info.sha256, await hashFile(archive));
  const stage = join(output, readdirSync(output).find(name => name.startsWith('.dsh-pack-')));
  assert.equal(readJson(join(stage, 'runtime/node_modules/@deepseek-ai/dsh/package.json')).version, pinned);
  assert.ok(existsSync(join(stage, 'src/history-reader.mjs')));
  assert.equal(existsSync(join(f.home, '.cutex/runtime/dsh/deployment.json')), false);
  assert.equal(existsSync(join(f.env.CUTEX_INSTALL_BASE, 'install.json')), false);
  console.log(JSON.stringify({ offlinePack: 'passed', sha256: info.sha256, nodeMinimum: info.nodeMinimum }));
  f.passed();
});
