import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

/**
 * ACP NDJSON transport. Requests are never retried after an uncertain write.
 * A `null` timeout waits for the answer (or the child's exit): a prompt has
 * no wall clock, only the runtime's liveness watchdog.
 */
export class AcpRpc {
  constructor(command, args, { cwd, env, update, permission, onExit = () => {}, onStream, timeout = 30000 }) {
    this.nextId = 0;
    this.lastActivity = Date.now();
    this.pending = new Map();
    this.timeout = timeout;
    this.update = update;
    this.permission = permission;
    // fd 3: display-only live stream frames from the cutex DSH plugin.
    this.child = spawn(command, args, { cwd, env: onStream ? { ...env, CUTEX_DSH_STREAM_FD: '3' } : env, windowsHide: true, stdio: onStream ? ['pipe', 'pipe', 'pipe', 'pipe'] : ['pipe', 'pipe', 'pipe'] });
    if (onStream) {
      const frames = createInterface({ input: this.child.stdio[3], crlfDelay: Infinity });
      frames.on('line', line => {
        if (Buffer.byteLength(line) > 1024 * 1024) return;
        try { onStream(JSON.parse(line)); } catch {}
      });
      this.child.stdio[3].on('error', () => {});
    }
    this.stderr = '';
    this.child.stderr.on('data', b => { this.stderr = (this.stderr + b).slice(-8192); });
    this.child.stdin.on('error', e => this.fail(e));
    this.child.on('error', e => this.fail(e));
    this.exited = new Promise(resolve => this.child.on('close', (code, signal) => {
      this.fail(new Error(`ACP exited (${code ?? signal})`));
      resolve();
      onExit(this);
    }));
    const lines = createInterface({ input: this.child.stdout, crlfDelay: Infinity });
    lines.on('line', line => {
      this.lastActivity = Date.now();
      try {
        if (Buffer.byteLength(line) > 8 * 1024 * 1024) throw new Error('ACP frame too large');
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
          if (message.error) pending.reject(Object.assign(new Error(message.error.message), { code: message.error.code }));
          else pending.resolve(message.result);
        }
      } catch (error) { this.fail(error); this.child.kill(); }
    });
  }

  fail(error) {
    this.dead = error;
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
      this.pending.set(id, { resolve, reject, timer });
      try { this.send({ id, method, params }); }
      catch (error) { clearTimeout(timer); this.pending.delete(id); reject(error); }
    });
  }

  async answer(message) {
    let result;
    if (message.method !== 'session/request_permission') {
      try { this.send({ id: message.id, error: { code: -32601, message: 'Unsupported client method' } }); } catch {}
      return;
    }
    try { result = await this.permission(message.params); }
    catch { result = { outcome: { outcome: 'cancelled' } }; }
    try { this.send({ id: message.id, result }); } catch {}
  }

  /** The watchdog's last resort: DSH stopped answering. */
  kill() { this.child.kill('SIGKILL'); }

  async close() {
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill(), 2000);
    await this.exited;
    clearTimeout(timer);
  }
}
