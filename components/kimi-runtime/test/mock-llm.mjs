// Scripted OpenAI/Kimi-compatible chat-completions mock for driving real `kimi acp`
// offline (no key, no network). The reply is decided from the LAST real user message
// (Kimi appends `<system-reminder>` user messages after it) and the tool results after it.
//
//   CALL <Tool> <json>          one tool call (`cutex:<tool>` = mcp__cutex__<tool>), then
//                               "tool done: <result>"
//   STEPS <json [{name,args}]>  several tool calls in order, then "steps done"
//   TODO                        TodoList with two items, then "todo done"
//   THINK                       streamed reasoning, then text
//   SLOW <ms>                   text streamed in 40 pieces, <ms> apart (interrupt tests)
//   SHELL-HOLD <marker>         Bash `sleep 600 && echo <marker>` (stop tests: a live tool process)
//   MODEL?                      "model=<model> thinking=<...>"
//   IMAGE?                      "image parts: <n>"
//   CONTEXT?                    "user turns: <n>" (owner turns Kimi sends the model)
//   MARKERS?                    counts of instruction markers in the system prompt
//   ... FAIL now (at the end)   HTTP 400 (the turn fails)
//   EXTERNAL_INPUT_JSON frames  "received <eventType>: <text>"
//   anything else               "echo: <text>"
import http from 'node:http';
import fs from 'node:fs';

const textOf = content => typeof content === 'string' ? content : Array.isArray(content) ? content.map(part => part.text ?? '').join('\n') : '';
const isReminder = message => message.role === 'user' && textOf(message.content).trimStart().startsWith('<system-reminder>');

function lastUserIndex(messages) {
  for (let i = messages.length - 1; i >= 0; i--) if (messages[i].role === 'user' && !isReminder(messages[i])) return i;
  return -1;
}
const toolName = name => name.startsWith('cutex:') ? `mcp__cutex__${name.slice(6)}` : name;

export function decide(body) {
  const messages = body.messages ?? [];
  const index = lastUserIndex(messages);
  const user = index >= 0 ? messages[index] : { content: '' };
  // Kimi merges a failed turn's prompt with the next one: the newest text part decides.
  const parts = Array.isArray(user.content) ? user.content.filter(part => typeof part.text === 'string' && !part.text.trimStart().startsWith('<system-reminder>')) : [];
  const text = parts.length ? parts.at(-1).text : textOf(user.content);
  const results = messages.slice(index + 1).filter(message => message.role === 'tool');
  const lastResult = results.length ? textOf(results.at(-1).content) : '';
  const system = messages.filter(message => message.role === 'system').map(message => textOf(message.content)).join('\n');
  const call = /(?:^|\n)CALL (\S+) (\{.*\})\s*$/s.exec(text);
  if (call) {
    if (results.length === 0) return { kind: 'tool', calls: [{ name: toolName(call[1]), args: JSON.parse(call[2]) }] };
    return { kind: 'text', text: `tool done: ${lastResult.slice(0, 400)}` };
  }
  const steps = /(?:^|\n)STEPS (\[.*\])\s*$/s.exec(text);
  if (steps) {
    const list = JSON.parse(steps[1]);
    if (results.length < list.length) return { kind: 'tool', calls: [{ name: toolName(list[results.length].name), args: list[results.length].args }] };
    return { kind: 'text', text: `steps done: ${lastResult.slice(0, 200)}` };
  }
  if (/\bTODO\b/.test(text)) {
    if (!results.length) return { kind: 'tool', calls: [{ name: 'TodoList', args: { todos: [{ title: 'first step', status: 'done' }, { title: 'second step', status: 'in_progress' }] } }] };
    return { kind: 'text', text: 'todo done' };
  }
  if (text.includes('THINK')) return { kind: 'text', reasoning: 'thinking about it step by step', text: 'MOCK-THOUGHT-REPLY' };
  // r267 B2: SHELL-HOLD <marker> runs a long shell command (its command line carries the
  // marker) until the turn is stopped; anywhere in the text, so it works inside ASK frames.
  const shell = /SHELL-HOLD ([\w-]+)/.exec(text);
  if (shell) {
    if (results.length === 0) return { kind: 'tool', calls: [{ name: 'Bash', args: { command: `sleep 600 && echo ${shell[1]}`, timeout: 600 } }] };
    return { kind: 'text', text: `shell done: ${lastResult.slice(0, 200)}` };
  }
  const slow = /SLOW (\d+)/.exec(text);
  if (slow) return { kind: 'text', text: Array.from({ length: 40 }, (_, i) => `slow-${i} `).join(''), pieces: 40, delay: Number(slow[1]) };
  if (text.includes('MODEL?')) return { kind: 'text', text: `model=${body.model} thinking=${JSON.stringify(body.thinking ?? body.reasoning_effort ?? null)}` };
  if (text.includes('IMAGE?')) {
    const parts = Array.isArray(user.content) ? user.content.filter(part => part.type === 'image_url').length : 0;
    return { kind: 'text', text: `image parts: ${parts}` };
  }
  if (text.includes('CONTEXT?')) {
    const owner = messages.filter(message => message.role === 'user' && !isReminder(message));
    return { kind: 'text', text: `user turns: ${owner.length} [${owner.map(message => textOf(message.content).slice(0, 24).replace(/\s+/g, ' ')).join(' | ')}]` };
  }
  if (text.includes('MARKERS?')) {
    const count = needle => system.split(needle).length - 1;
    return { kind: 'text', text: `markers cutex=${count('# Cutex')} user=${count('USER-AGENTS-MARKER')} project=${count('PROJECT-AGENTS-MARKER')} files=${count('# File references')}` };
  }
  if (/FAIL now\s*$/.test(text)) return { kind: 'error' };
  if (text.includes('EXTERNAL_INPUT_JSON')) {
    const frames = [...text.matchAll(/EXTERNAL_INPUT_JSON\n(\{.*?\})(?=\n\nEXTERNAL_INPUT_JSON|\n\n|$)/gs)].map(match => { try { return JSON.parse(match[1]); } catch { return null; } }).filter(Boolean);
    const frame = frames.at(-1) ?? {};
    return { kind: 'text', text: `received ${frame.eventType}: ${String(frame.text ?? '').slice(0, 300)}` };
  }
  return { kind: 'text', text: `echo: ${text.slice(0, 200)}` };
}

const delay = ms => new Promise(done => setTimeout(done, ms));

export function startMock({ port = 0, host = '127.0.0.1', logFile } = {}) {
  const requests = [];
  let callSeq = 0;
  const server = http.createServer((req, res) => {
    let raw = '';
    req.on('data', chunk => (raw += chunk));
    req.on('end', async () => {
      let body = {};
      try { body = raw ? JSON.parse(raw) : {}; } catch {}
      const entry = { t: Date.now(), method: req.method, url: req.url, model: body.model, auth: req.headers.authorization ? 'present' : 'absent', tools: (body.tools ?? []).map(tool => tool.function?.name ?? tool.name) };
      requests.push({ ...entry, body });
      if (logFile) fs.appendFileSync(logFile, JSON.stringify(entry) + '\n');
      if (req.method === 'GET' && /\/models\/?$/.test(req.url)) {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ object: 'list', data: [{ id: 'mock-kimi', object: 'model', context_length: 262144 }] }));
        return;
      }
      if (req.method !== 'POST' || !/chat\/completions/.test(req.url)) {
        res.writeHead(404, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ error: { message: 'mock: not found', type: 'not_found' } }));
        return;
      }
      const step = decide(body);
      if (step.kind === 'error') {
        res.writeHead(400, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ error: { message: 'MOCK-FAILURE: scripted provider error', type: 'invalid_request_error' } }));
        return;
      }
      const id = 'chatcmpl-mock', model = body.model ?? 'mock-kimi';
      const chunk = (delta, finish = null, extra = {}) => `data: ${JSON.stringify({ id, object: 'chat.completion.chunk', created: 0, model, choices: [{ index: 0, delta, finish_reason: finish }], ...extra })}\n\n`;
      const usage = { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 };
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
      res.write(chunk({ role: 'assistant', content: '' }));
      let aborted = false;
      res.on('close', () => { aborted = true; });
      if (step.kind === 'text') {
        if (step.reasoning) for (const piece of step.reasoning.match(/.{1,6}/gs)) res.write(chunk({ reasoning_content: piece }));
        const pieces = step.pieces ? step.text.match(new RegExp(`.{1,${Math.ceil(step.text.length / step.pieces)}}`, 'gs')) : step.text.match(/.{1,8}/gs);
        for (const piece of pieces) {
          if (aborted) return;
          res.write(chunk({ content: piece }));
          if (step.delay) await delay(step.delay);
        }
        res.write(chunk({}, 'stop', { usage }));
      } else {
        step.calls.forEach((call, index) => {
          res.write(chunk({ tool_calls: [{ index, id: `call_${++callSeq}`, type: 'function', function: { name: call.name, arguments: '' } }] }));
          res.write(chunk({ tool_calls: [{ index, function: { arguments: JSON.stringify(call.args) } }] }));
        });
        res.write(chunk({}, 'tool_calls', { usage }));
      }
      res.write('data: [DONE]\n\n');
      res.end();
    });
  });
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, host, () => resolve({ server, port: server.address().port, requests, close: () => new Promise(done => { server.closeAllConnections?.(); server.close(done); }) }));
  });
}
