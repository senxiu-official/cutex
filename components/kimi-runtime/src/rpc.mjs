import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

/**
 * ACP NDJSON transport to `kimi acp`. Requests are never retried after an
 * uncertain write. A `null` timeout waits for the answer (or the child's
 * exit): a prompt has no wall clock, only the runtime's liveness watchdog.
 */
export class AcpRpc {
  constructor(command, args, { cwd, env, update, permission, onExit = () => {}, timeout = 30000, stopGrace = 5000 }) {
    this.nextId = 0;
    this.lastActivity = Date.now();
    this.pending = new Map();
    this.timeout = timeout;
    this.stopGrace = stopGrace;
    this.update = update;
    this.permission = permission;
    this.child = spawn(command, args, { cwd, env, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
    this.stderr = '';
    this.child.stderr.on('data', b => { this.stderr = (this.stderr + b).slice(-8192); });
    this.child.stdin.on('error', e => this.fail(e));
    this.child.on('error', e => this.fail(e));
    this.exited = new Promise(resolve => this.child.on('close', (code, signal) => {
      this.exitStatus = { code, signal };
      this.fail(new Error(`Kimi ACP exited (${code ?? signal})`));
      resolve();
      onExit(this);
    }));
    const lines = createInterface({ input: this.child.stdout, crlfDelay: Infinity });
    lines.on('line', line => {
      this.lastActivity = Date.now();
      if (!line.trim()) return;
      try {
        if (Buffer.byteLength(line) > 16 * 1024 * 1024) throw new Error('ACP frame too large');
        const message = JSON.parse(line);
        if (message.jsonrpc !== '2.0') throw new Error('invalid ACP envelope');
        if (message.method) {
          if (message.id !== undefined) void this.answer(message);
          else if (message.method === 'session/update') this.update(message.params);
        } else {
          const pending = this.pending.get(message.id);
          if (!pending) return;
          this.pending.delete(message.id);
          clearTimeout(pending.timer);
          if (message.error) pending.reject(Object.assign(new Error(message.error.message), { code: message.error.code, data: message.error.data }));
          else pending.resolve(message.result);
        }
      } catch (error) { this.fail(error); this.child.kill(); }
    });
  }

  fail(error) {
    this.dead ??= error;
    for (const p of this.pending.values()) { clearTimeout(p.timer); p.reject(error); }
    this.pending.clear();
  }

  send(message) {
    if (this.dead) throw this.dead;
    this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...message }) + '\n');
  }

  request(method, params, timeout = this.timeout) {
    if (this.dead) return Promise.reject(this.dead);
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      const timer = timeout == null ? null : setTimeout(() => {
        this.pending.delete(id);
        reject(Object.assign(new Error(`ACP ${method} timed out; outcome unknown`), { timedOut: true }));
      }, timeout);
      timer?.unref?.();
      this.pending.set(id, { resolve, reject, timer });
      try { this.send({ id, method, params }); }
      catch (error) { clearTimeout(timer); this.pending.delete(id); reject(error); }
    });
  }

  /** Reverse requests: approvals (and Kimi's question bridge) only. */
  async answer(message) {
    if (message.method !== 'session/request_permission') {
      try { this.send({ id: message.id, error: { code: -32601, message: 'Unsupported client method' } }); } catch {}
      return;
    }
    let result;
    try { result = await this.permission(message.params); }
    catch { result = { outcome: { outcome: 'cancelled' } }; }
    try { this.send({ id: message.id, result }); } catch {}
  }

  /** The watchdog's last resort: Kimi stopped answering. */
  kill() { this.child.kill('SIGKILL'); }

  /**
   * Graceful stop: Kimi writes its session log asynchronously after a turn
   * settles, and a SIGTERM right after a prompt loses the turn's tail. Closing
   * stdin lets it flush and exit on its own; only then SIGTERM, then SIGKILL.
   */
  async close() {
    if (this.exitStatus) return;
    try { this.child.stdin.end(); } catch {}
    const term = setTimeout(() => { try { this.child.kill('SIGTERM'); } catch {} }, this.stopGrace);
    const kill = setTimeout(() => { try { this.child.kill('SIGKILL'); } catch {} }, this.stopGrace + 3000);
    await this.exited;
    clearTimeout(term); clearTimeout(kill);
  }
}
