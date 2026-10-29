// Fake Kimi OAuth device-code + /coding/v1/models server (K2 tests only; never real Kimi).
// Usage: node fake-kimi-oauth.mjs [--port N] [--pending N] [--deny] [--expires-in S] [--refresh-delay MS]
//   [--token-prefix P] [--capture FILE]
// --token-prefix: tokens are `<P>-access-<n>` / `<P>-refresh-<n>` (default `fake`), so a test can
//   grep one recognisable canary prefix for every generation (r267 B1 canary).
// --capture: append every chat-completions request body (what the model sees) as a JSON line.
// A newest user text whose last line contains FAKE-PROVIDER-ERROR gets a 400 error instead of a reply.
// Refresh tokens rotate: a refresh with a superseded refresh token gets 401 invalid_grant (like Kimi).
// POST /coding/v1/chat/completions streams a short reply for any issued access token;
// a newest user text ending in `CALL <Tool> <json>` gets that one tool call first
// (then the reply), so a test can make Kimi's own tools read a file.
// Prints {"port":N} on stdout once listening. Logs requests (without bodies) to stderr.
import http from 'node:http';
import { appendFileSync } from 'node:fs';
const args = process.argv.slice(2);
const opt = (name, dflt) => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : dflt; };
const port = Number(opt('--port', 0));
const pendingPolls = Number(opt('--pending', 2));
const deny = args.includes('--deny');
const expiresIn = Number(opt('--expires-in', 3600));
const refreshDelay = Number(opt('--refresh-delay', 0));
const prefix = opt('--token-prefix', 'fake');
const capture = opt('--capture', null);
const issued = new Set(); let rejectedRefreshes = 0; let chats = 0;
let polls = 0; let gen = 0; let currentRefresh = null; let refreshes = 0;
const tokens = () => { gen += 1; currentRefresh = `${prefix}-refresh-${gen}`; issued.add(`${prefix}-access-${gen}`); return { access_token: `${prefix}-access-${gen}`, refresh_token: currentRefresh, expires_in: expiresIn, scope: 'kimi-code', token_type: 'Bearer' }; };
const send = (res, status, body) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
const server = http.createServer((req, res) => {
  let raw = ''; req.on('data', (c) => { raw += c; });
  req.on('end', () => {
    const form = new URLSearchParams(raw);
    const base = `http://127.0.0.1:${server.address().port}`;
    process.stderr.write(`${req.method} ${req.url} ${form.get('grant_type') ?? ''}\n`);
    if (req.method === 'POST' && req.url === '/api/oauth/device_authorization') {
      return send(res, 200, { user_code: 'FAKE-1234', device_code: 'fake-device', verification_uri: `${base}/device`, verification_uri_complete: `${base}/device?user_code=FAKE-1234`, expires_in: 600, interval: 1 });
    }
    if (req.method === 'POST' && req.url === '/api/oauth/token') {
      if (form.get('grant_type') === 'urn:ietf:params:oauth:grant-type:device_code') {
        if (deny) return send(res, 400, { error: 'access_denied', error_description: 'owner said no' });
        polls += 1;
        if (polls <= pendingPolls) return send(res, 400, { error: 'authorization_pending' });
        return send(res, 200, tokens());
      }
      if (form.get('grant_type') === 'refresh_token') {
        const presented = form.get('refresh_token');
        const answer = () => {
          if (presented !== currentRefresh) { rejectedRefreshes += 1; return send(res, 401, { error: 'invalid_grant' }); }
          refreshes += 1; return send(res, 200, tokens());
        };
        // The delay widens the race window between two refreshing processes.
        return refreshDelay > 0 ? setTimeout(answer, refreshDelay) : answer();
      }
    }
    if (req.method === 'GET' && req.url === '/coding/v1/models') {
      if (!issued.has((req.headers.authorization ?? '').replace(/^Bearer /, ''))) return send(res, 401, { error: 'unauthorized' });
      return send(res, 200, { data: [
        { id: 'kimi-for-coding', display_name: 'Kimi For Coding', limit: { context: 262144 }, modalities: { input: ['text', 'image'] },
          reasoning: { type: 'both', effort: { valid: ['low', 'high', 'max'], default: 'high' } } },
        { id: 'kimi-k3-fast', display_name: 'Kimi K3 Fast', limit: { context: 131072 }, modalities: { input: ['text'] } },
      ] });
    }
    if (req.method === 'POST' && req.url === '/coding/v1/chat/completions') {
      const token = (req.headers.authorization ?? '').replace(/^Bearer /, '');
      if (!issued.has(token)) return send(res, 401, { error: { message: 'unauthorized', type: 'invalid_authentication_error' } });
      chats += 1;
      if (capture) appendFileSync(capture, raw.replace(/\n/g, ' ') + '\n');
      let body = {}; try { body = JSON.parse(raw); } catch { /* ignore */ }
      const chunk = (delta, finish = null, extra = {}) => `data: ${JSON.stringify({ id: 'chatcmpl-fake', object: 'chat.completion.chunk', created: 0, model: body.model ?? 'kimi-for-coding', choices: [{ index: 0, delta, finish_reason: finish }], ...extra })}\n\n`;
      const messages = Array.isArray(body.messages) ? body.messages : [];
      const textOf = content => typeof content === 'string' ? content : Array.isArray(content) ? content.map(part => part?.text ?? '').join('\n') : '';
      let user = -1;
      for (let i = messages.length - 1; i >= 0; i--) if (messages[i].role === 'user' && !textOf(messages[i].content).trimStart().startsWith('<system-reminder>')) { user = i; break; }
      // Only the newest text part: Kimi merges a failed turn's user text with the next one.
      if (user >= 0 && textOf(messages[user].content).trim().split('\n').at(-1).includes('FAKE-PROVIDER-ERROR')) {
        return send(res, 400, { error: { message: 'fake provider rejected the request', type: 'invalid_request_error' } });
      }
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
      res.write(chunk({ role: 'assistant', content: '' }));
      const call = user >= 0 ? /(?:^|\n)CALL (\S+) (\{.*\})\s*$/s.exec(textOf(messages[user].content)) : null;
      if (call && !messages.slice(user + 1).some(message => message.role === 'tool')) {
        res.write(chunk({ tool_calls: [{ index: 0, id: `call_${chats}`, type: 'function', function: { name: call[1], arguments: '' } }] }));
        res.write(chunk({ tool_calls: [{ index: 0, function: { arguments: call[2] } }] }));
        res.write(chunk({}, 'tool_calls', { usage: { prompt_tokens: 10, completion_tokens: 2, total_tokens: 12 } }));
        res.write('data: [DONE]\n\n');
        return res.end();
      }
      res.write(chunk({ content: 'FAKE-KIMI-REPLY' }));
      res.write(chunk({}, 'stop', { usage: { prompt_tokens: 10, completion_tokens: 2, total_tokens: 12 } }));
      res.write('data: [DONE]\n\n');
      return res.end();
    }
    if (req.url === '/stats') return send(res, 200, { polls, gen, refreshes, rejectedRefreshes, chats });
    send(res, 404, { error: 'not_found' });
  });
});
server.listen(port, '127.0.0.1', () => { process.stdout.write(JSON.stringify({ port: server.address().port }) + '\n'); });
process.stdin.on('end', () => process.exit(0)); process.stdin.resume();
