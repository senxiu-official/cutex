// Redaction of credentials from everything cutex records about a Kimi agent
// (r266 B1). Kimi itself can read its credentials (as the official CLI can);
// cutex keeps them out of its own events, queries, timeline, history and logs.
//
// A redaction source is { literals(), fingerprints() }:
// - literals: values this process knows (the launch token, secret-named env
//   values, retired API keys cutex passes in KIMI_CUTEX_REDACT, and the Kimi
//   account's OAuth tokens read from the agent's linked `credentials/`
//   directory, re-read whenever a file there changes, plus a bounded list of
//   values seen before a refresh);
// - fingerprints: { length, sha256 } of rotated values cutex no longer holds
//   (the account's redaction ledger, written by the daemon and by adapters).
//   A token-shaped run of text whose length and SHA-256 match is redacted.
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, statSync, writeFileSync, renameSync, mkdirSync, unlinkSync } from 'node:fs';
import { join, dirname, isAbsolute } from 'node:path';

export const REDACTED = '[REDACTED]';
/** Shorter values are never fingerprinted or read from a credential file. */
export const MIN_SECRET_LENGTH = 8;
/** Values seen in the credential files before a change, kept for this process. */
export const RETAINED_MAX = 16;
/** Ledger entries kept (the daemon keeps the same bound). */
export const LEDGER_MAX = 64;

const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const sha256 = text => createHash('sha256').update(text, 'utf8').digest('hex');

export function fingerprintOf(value) {
  return typeof value === 'string' && value.length >= MIN_SECRET_LENGTH ? { length: Buffer.byteLength(value, 'utf8'), sha256: sha256(value) } : null;
}

// `[A-Za-z0-9._~+/=-]` (base64, JWT, URL-safe) and the narrower `[A-Za-z0-9._~-]`
// (a token after `=` or between `/`): the same classes as src/runtime/kimi/redaction.rs.
const alnum = c => (c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122);
const NARROW = c => alnum(c) || c === 46 || c === 95 || c === 126 || c === 45;
const WIDE = c => NARROW(c) || c === 43 || c === 47 || c === 61;
const CLASSES = [WIDE, NARROW];

/** Ranges [start, end) of `text` that are a fingerprinted value (ASCII runs only). */
function fingerprintRanges(text, fingerprints) {
  if (!fingerprints.size) return [];
  const lengths = new Set([...fingerprints.values()].map(fp => fp.length));
  const ranges = [];
  for (const cls of CLASSES) {
    let start = -1;
    for (let index = 0; index <= text.length; index++) {
      const inside = index < text.length && cls(text.charCodeAt(index));
      if (inside && start < 0) start = index;
      else if (!inside && start >= 0) {
        if (lengths.has(index - start) && fingerprints.has(`${index - start}:${sha256(text.slice(start, index))}`)) ranges.push([start, index]);
        start = -1;
      }
    }
  }
  return ranges;
}

/** One string with every literal and fingerprinted value replaced. */
export function redactText(text, literals, fingerprints) {
  if (typeof text !== 'string' || !text) return text;
  const ranges = [];
  for (const literal of literals) {
    let from = 0, at;
    while ((at = text.indexOf(literal, from)) !== -1) { ranges.push([at, at + literal.length]); from = at + literal.length; }
  }
  ranges.push(...fingerprintRanges(text, fingerprints));
  if (!ranges.length) return text;
  ranges.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  let out = '', at = 0, end = -1;
  for (const [start, stop] of ranges) {
    if (start < end) { end = Math.max(end, stop); continue; }
    if (end >= 0) at = end;
    out += text.slice(at, start) + REDACTED;
    end = stop;
  }
  return out + text.slice(end);
}

const keyOf = fp => `${fp.length}:${fp.sha256}`;

/** Credential-shaped string values of a credential file (the OAuth token bundle). */
function credentialValues(raw) {
  let parsed;
  try { parsed = JSON.parse(raw); } catch { return []; }
  const values = [];
  const walk = value => {
    if (typeof value === 'string') { if (value.length >= MIN_SECRET_LENGTH && !/\s/.test(value)) values.push(value); return; }
    if (Array.isArray(value)) value.forEach(walk);
    else if (object(value)) for (const [key, entry] of Object.entries(value)) {
      // Token bundles: access / refresh / id tokens, keys and secrets; never scopes or types.
      if (typeof entry === 'string' && !/token|secret|key|password|credential/i.test(key)) continue;
      walk(entry);
    }
  };
  walk(parsed);
  return values;
}

/**
 * The redaction source of one Kimi agent (or reader). `literals`: fixed values;
 * `credentialsDir`: the agent's (linked) Kimi `credentials/` directory, re-read
 * when it changes; `ledger`: the account's fingerprint ledger file.
 */
export function redactionSource({ literals = [], credentialsDir, ledger } = {}) {
  const fixed = [...new Set(literals.filter(value => typeof value === 'string' && value.length))];
  let signature = '', current = [], retained = [], ledgerSignature = '', ledgerSet = new Map(), observed = [];
  const statKey = path => { try { const s = statSync(path); return `${s.mtimeMs}:${s.size}:${s.ino}`; } catch { return '-'; } };
  function readLedger() {
    if (!ledger || !isAbsolute(ledger)) return;
    const next = statKey(ledger);
    if (next === ledgerSignature) return;
    ledgerSignature = next;
    const map = new Map();
    try {
      for (const fp of JSON.parse(readFileSync(ledger, 'utf8'))) {
        if (object(fp) && Number.isSafeInteger(fp.length) && typeof fp.sha256 === 'string' && /^[0-9a-f]{64}$/.test(fp.sha256)) map.set(keyOf(fp), fp);
      }
    } catch {}
    ledgerSet = map;
  }
  /** Adds what this process saw to the ledger (merge + atomic replace; best effort). */
  function recordInLedger(values) {
    if (!ledger || !isAbsolute(ledger)) return;
    const fps = values.map(fingerprintOf).filter(Boolean);
    readLedger();
    if (fps.every(fp => ledgerSet.has(keyOf(fp)))) return;
    let list = [];
    try { list = JSON.parse(readFileSync(ledger, 'utf8')); if (!Array.isArray(list)) list = []; } catch {}
    for (const fp of fps) if (!list.some(entry => object(entry) && entry.length === fp.length && entry.sha256 === fp.sha256)) list.push(fp);
    list = list.slice(-LEDGER_MAX);
    const temporary = `${ledger}.${process.pid}.${Date.now()}.tmp`;
    try {
      mkdirSync(dirname(ledger), { recursive: true, mode: 0o700 });
      writeFileSync(temporary, JSON.stringify(list), { mode: 0o600, flag: 'wx' });
      renameSync(temporary, ledger);
    } catch { try { unlinkSync(temporary); } catch {} }
    ledgerSignature = '';
    readLedger();
  }
  function readCredentials() {
    if (!credentialsDir || !isAbsolute(credentialsDir)) return;
    let names;
    try { names = readdirSync(credentialsDir).filter(name => name.endsWith('.json')).sort(); } catch { names = []; }
    const next = names.map(name => `${name}=${statKey(join(credentialsDir, name))}`).join('|');
    if (next === signature) return;
    signature = next;
    const values = [];
    for (const name of names) {
      try { values.push(...credentialValues(readFileSync(join(credentialsDir, name), 'utf8'))); } catch {}
    }
    const fresh = [...new Set(values)];
    for (const old of current) if (!fresh.includes(old) && !retained.includes(old)) retained.push(old);
    retained = retained.slice(-RETAINED_MAX);
    current = fresh;
    const unseen = fresh.filter(value => !observed.includes(value));
    if (unseen.length) { observed = [...observed, ...unseen].slice(-RETAINED_MAX * 2); recordInLedger(unseen); }
  }
  return {
    literals() { readCredentials(); return [...new Set([...fixed, ...current, ...retained])].sort((a, b) => b.length - a.length); },
    fingerprints() { readLedger(); return ledgerSet; },
  };
}

/** A fixed list or a source, as a source. */
export function asSource(secrets) {
  if (secrets && typeof secrets.literals === 'function' && typeof secrets.fingerprints === 'function') return secrets;
  return redactionSource({ literals: Array.isArray(secrets) ? secrets : [] });
}

/** Deep redaction of a JSON value (keys too). */
export function secretRedactor(secrets = []) {
  const source = asSource(secrets);
  return value => {
    const literals = source.literals();
    const fingerprints = source.fingerprints();
    if (!literals.length && !fingerprints.size) return value;
    const redact = entry => {
      if (typeof entry === 'string') return redactText(entry, literals, fingerprints);
      if (Array.isArray(entry)) return entry.map(redact);
      if (object(entry)) return Object.fromEntries(Object.entries(entry).map(([key, inner]) => [redact(key), redact(inner)]));
      return entry;
    };
    return redact(value);
  };
}

/**
 * Redaction of one streamed text (an item's deltas), safe across chunk
 * boundaries: `push(chunk)` returns what may go out now, already redacted, and
 * holds back the longest tail that could still grow into a secret: a proper
 * prefix of a literal, or (when fingerprints exist) the trailing token-shaped
 * run that could still become a fingerprinted value. `flush()` returns that
 * tail at the end of the item. Returns a factory: one state per item.
 */
export function streamRedactor(secrets = []) {
  const source = asSource(secrets);
  const heldLength = (text, literals, fingerprints) => {
    let longest = 0;
    for (const secret of literals) {
      for (let length = Math.min(secret.length - 1, text.length); length > longest; length--) {
        if (text.endsWith(secret.slice(0, length))) { longest = length; break; }
      }
    }
    if (fingerprints.size) {
      const max = Math.max(...[...fingerprints.values()].map(fp => fp.length));
      for (const cls of CLASSES) {
        let run = 0;
        while (run < text.length && cls(text.charCodeAt(text.length - 1 - run))) run++;
        if (run > 0 && run <= max) { longest = Math.max(longest, run); break; }
      }
    }
    return longest;
  };
  return () => {
    let held = '';
    return {
      push(chunk) {
        const literals = source.literals();
        const fingerprints = source.fingerprints();
        const pending = redactText(held + chunk, literals, fingerprints);
        const keep = heldLength(pending, literals, fingerprints);
        held = pending.slice(pending.length - keep);
        return pending.slice(0, pending.length - keep);
      },
      flush() {
        const tail = redactText(held, source.literals(), source.fingerprints());
        held = '';
        return tail;
      },
    };
  };
}

/**
 * A reader's / session operation's source from its environment: retired keys
 * in KIMI_CUTEX_REDACT (DSH_CUTEX_REDACT), the credential directory and ledger
 * paths in KIMI_CUTEX_REDACT_SOURCES (never values).
 */
export function sourceFromEnv(env = process.env) {
  let literals = [];
  try { literals = JSON.parse(env.KIMI_CUTEX_REDACT ?? env.DSH_CUTEX_REDACT ?? '[]'); } catch {}
  let paths = {};
  try { paths = JSON.parse(env.KIMI_CUTEX_REDACT_SOURCES ?? '{}'); } catch {}
  return redactionSource({ literals: Array.isArray(literals) ? literals : [], ...redactionPaths(paths) });
}

/** The `{ credentialsDir, ledger }` of a launch file / env value (absolute paths only). */
export function redactionPaths(value) {
  const path = entry => typeof entry === 'string' && isAbsolute(entry) ? entry : undefined;
  return object(value) ? { credentialsDir: path(value.credentialsDir), ledger: path(value.ledger) } : {};
}
