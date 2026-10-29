import { open, stat, readdir, lstat } from 'node:fs/promises';
import { basename, dirname, isAbsolute, join, resolve } from 'node:path';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { zstdDecompressSync } from 'node:zlib';
import { attachmentMetadata, attachmentPlaceholder, nativeBlocks } from './nativeProjection.mjs';

const rowAdmissions = new WeakMap();

/** Load the codec belonging to the controller's pinned DSH installation (no DSH process). */
export async function loadNativeCatalog(installManifest = new URL('../../../.dsh-install/package.json', import.meta.url)) {
  const require = createRequire(installManifest);
  const catalogPath = require.resolve('@deepseek-ai/dsh-session-format-catalog');
  const catalog = (await import(pathToFileURL(catalogPath).href)).sessionFormatCatalog;
  if (!rowAdmissions.has(catalog)) {
    // The tail reader validates one row at a time with the SAME pinned v4 row
    // admission the DSH JSONL scanner applies (format.ts consumeEventLine).
    let admission = null;
    try {
      const codec = await import(pathToFileURL(createRequire(catalogPath).resolve('@deepseek-ai/dsh-session-format-v3-to-v4')).href);
      if (typeof codec.assertV4RowAdmission === 'function') admission = codec.assertV4RowAdmission;
    } catch { /* structural row validation still applies */ }
    rowAdmissions.set(catalog, admission);
  }
  return catalog;
}

const digest = value => createHash('sha256').update(value).digest('hex');

/**
 * Budgets of the whole-prefix snapshot reader readNativeLog (diagnostics/tests).
 * The product path (reconcileNativeLog) is an incremental tail reader and has
 * no whole-log limit; see NATIVE_TAIL_LIMITS.
 */
export const NATIVE_LOG_LIMITS = Object.freeze({
  maxFileBytes: 32 * 1024 * 1024,
  maxDecompressedBytes: 64 * 1024 * 1024,
  maxEvents: 100000,
});
function scanLimits(overrides = {}) {
  if (!overrides || typeof overrides !== 'object' || Array.isArray(overrides)) throw new Error('Invalid native log limits');
  const limits = { ...NATIVE_LOG_LIMITS, ...overrides };
  for (const [name, value] of Object.entries(limits)) {
    if (!Object.hasOwn(NATIVE_LOG_LIMITS, name) || !Number.isSafeInteger(value) || value < 1) throw new Error(`Invalid native log limit: ${name}`);
  }
  return limits;
}
function limitExceeded(name) {
  return Object.assign(new Error(`Native log exceeds ${name}; full-prefix recovery refused`), { code: 'DSH_NATIVE_LOG_LIMIT', limit: name });
}
const changedDuringScan = () => Object.assign(new Error('Native log changed during scan; retry with a fresh prefix'), { code: 'DSH_NATIVE_LOG_CHANGED' });
const fingerprint = metadata => [metadata.dev, metadata.ino, metadata.size, metadata.mtimeNs, metadata.ctimeNs, metadata.birthtimeNs].join(':');
function checkFile(metadata, limits) {
  if (!metadata.isFile()) throw new Error('Native log must be a regular file');
  if (metadata.size > BigInt(limits.maxFileBytes)) throw limitExceeded('maxFileBytes');
}

/** Fixed-size positioned reads, rather than readFile (which can follow growth). */
async function boundedRead(logPath, limits) {
  const handle = await open(logPath, 'r');
  try {
    const before = await handle.stat({ bigint: true });
    checkFile(before, limits);
    const input = Buffer.alloc(Number(before.size));
    let offset = 0;
    while (offset < input.length) {
      const { bytesRead } = await handle.read(input, offset, input.length - offset, offset);
      if (!bytesRead) throw changedDuringScan();
      offset += bytesRead;
    }
    const after = await handle.stat({ bigint: true });
    const current = await stat(logPath, { bigint: true });
    if (fingerprint(before) !== fingerprint(after) || fingerprint(before) !== fingerprint(current)) throw changedDuringScan();
    return { input, fingerprint: fingerprint(before) };
  } finally { await handle.close(); }
}

// DSH persistence format.ts encodeSegment: operate on UTF-16 code units,
// including lone surrogates. Never interpolate an unencoded native session ID.
function encodeSegment(value) {
  if (typeof value !== 'string' || !value.length) throw new Error('Native session ID must be nonempty');
  if (value === '.' || value === '..') return '~002E'.repeat(value.length);
  let encoded = '';
  for (let i = 0; i < value.length; i++) {
    const char = value[i];
    encoded += /^[A-Za-z0-9._-]$/.test(char) ? char : `~${value.charCodeAt(i).toString(16).toUpperCase().padStart(4, '0')}`;
  }
  return encoded;
}

/**
 * Discover <explicit-root>/<project>/<encoded-id>/session.v4.jsonl[.zstd].
 * Mirrors DSH findLog/resolveGenerationInDirectory: highest canonical format,
 * unique session across projects, no temporary files or legacy flat layout.
 * Returns null on ENOENT/not-yet-materialized; caller retries after a later
 * notification or bounded poll. Never reads any inferred home or follows child
 * directory/file symlinks. readNativeLog validates the selected header identity.
 */
export async function locateNativeLog(root, sessionId) {
  if (typeof root !== 'string' || !isAbsolute(root)) throw new Error('Explicit absolute native persistence root required');
  const encoded = encodeSegment(sessionId);
  const list = async path => {
    try { return await readdir(path, { withFileTypes: true }); }
    catch (error) { if (error.code === 'ENOENT') return []; throw error; }
  };
  const matches = [];
  for (const project of await list(root)) {
    if (!project.isDirectory() || project.isSymbolicLink()) continue;
    const projectPath = join(root, project.name);
    for (const suffix of ['.jsonl', '.jsonl.zstd']) {
      try {
        await lstat(join(projectPath, encoded + suffix));
        throw new Error('Legacy flat native session log requires DSH migration');
      } catch (error) { if (error.code !== 'ENOENT') throw error; }
    }
    const directory = join(projectPath, encoded);
    try {
      const stat = await lstat(directory);
      if (stat.isSymbolicLink()) throw new Error('Native session directory must not be a symlink');
      if (!stat.isDirectory()) throw new Error('Native session path is not a directory');
    } catch (error) { if (error.code === 'ENOENT') continue; throw error; }
    const generations = [];
    for (const entry of await list(directory)) {
      const match = /^session(?:\.v([1-9][0-9]*))?\.jsonl(\.zstd)?$/.exec(entry.name);
      if (!match) continue;
      if (!entry.isFile() || entry.isSymbolicLink()) throw new Error('Native log must be a regular file');
      const version = Number(match[1] ?? 0);
      if (!Number.isSafeInteger(version)) throw new Error('Invalid native generation version');
      generations.push({ path: join(directory, entry.name), version, compressed: Boolean(match[2]) });
    }
    if (new Set(generations.map(entry => entry.compressed)).size > 1) throw new Error('Conflicting native log compression modes');
    generations.sort((a, b) => b.version - a.version);
    if (generations.length) matches.push(generations[0].path);
  }
  if (matches.length > 1) throw new Error('Duplicate native session across project directories');
  return matches[0] ?? null;
}

/**
 * DSH 0.2.1-rc.3 uses checksummed, independently compressed header/batch frames.
 * Inspect framing before decompression: a torn final frame is NOT a committed
 * batch, even if its prefix happens to decompress to complete JSONL records.
 * Based on read-only DSH packages/session/session-persistence-jsonl/src/zstd.ts.
 */
function decodeFrames(bytes, maxDecompressedBytes) {
  const output = [];
  let outputBytes = 0;
  let offset = 0;
  while (offset < bytes.length) {
    const start = offset;
    const torn = () => ({ bytes: Buffer.concat(output), tornBytes: bytes.length - start });
    if (bytes.length - offset < 4) return torn();
    if (bytes.readUInt32LE(offset) !== 0xfd2fb528) throw new Error(`Invalid Zstandard magic at ${offset}`);
    offset += 4;
    if (offset === bytes.length) return torn();
    const descriptor = bytes[offset++];
    if (descriptor & 0x18) throw new Error('Reserved Zstandard header bit');
    const single = Boolean(descriptor & 0x20);
    const sizeFlag = descriptor >>> 6;
    const dictionary = descriptor & 3;
    const headerBytes = (single ? 0 : 1) + (dictionary === 3 ? 4 : dictionary)
      + (sizeFlag === 0 ? (single ? 1 : 0) : 1 << sizeFlag);
    if (bytes.length - offset < headerBytes) return torn();
    offset += headerBytes;
    for (;;) {
      if (bytes.length - offset < 3) return torn();
      const block = bytes.readUIntLE(offset, 3);
      offset += 3;
      const type = (block >>> 1) & 3;
      if (type === 3) throw new Error('Reserved Zstandard block type');
      const length = type === 1 ? 1 : block >>> 3;
      if (bytes.length - offset < length) return torn();
      offset += length;
      if (block & 1) break;
    }
    const checksumBytes = descriptor & 4 ? 4 : 0;
    if (bytes.length - offset < checksumBytes) return torn();
    offset += checksumBytes;
    // The public Node decoder verifies the checksum for each complete frame.
    const remaining = maxDecompressedBytes - outputBytes;
    if (remaining <= 0) throw limitExceeded('maxDecompressedBytes');
    let decoded;
    try { decoded = zstdDecompressSync(bytes.subarray(start, offset), { maxOutputLength: remaining }); }
    catch (error) {
      if (error.code === 'ERR_BUFFER_TOO_LARGE') throw limitExceeded('maxDecompressedBytes');
      throw error;
    }
    outputBytes += decoded.length;
    if (outputBytes > maxDecompressedBytes) throw limitExceeded('maxDecompressedBytes');
    output.push(decoded);
  }
  return { bytes: Buffer.concat(output), tornBytes: 0 };
}

/**
 * Read only an explicitly selected canonical v4 artifact. Never opens a writer,
 * lease, migration, session service or the real user's DSH home. Caller resolves
 * the path from its private DSH persistence config; no home-directory guessing.
 * Complete newline-terminated rows are admitted, torn final records ignored;
 * malformed complete rows refuse the entire scan rather than skipping a gap.
 * Historical migration is deliberately refused: it can renumber native events.
 * Whole-prefix snapshot for diagnostics and tests only; it loads every event and
 * obeys NATIVE_LOG_LIMITS. The product path is reconcileNativeLog.
 */
export async function readNativeLog({ logPath, sessionId, catalog, limits: overrides, compression = logPath.endsWith('.zstd') ? 'zstd' : 'none' }) {
  const limits = scanLimits(overrides);
  catalog ??= await loadNativeCatalog();
  const { input, fingerprint: fileFingerprint } = await boundedRead(logPath, limits);
  if (!['none', 'zstd'].includes(compression)) throw new Error('Unknown native log compression');
  if (compression === 'none' && input.length > limits.maxDecompressedBytes) throw limitExceeded('maxDecompressedBytes');
  const decoded = compression === 'zstd' ? decodeFrames(input, limits.maxDecompressedBytes) : { bytes: input, tornBytes: 0 };
  const end = decoded.bytes.lastIndexOf(10);
  if (end < 0) throw new Error('Native session header is incomplete');
  const headerEnd = decoded.bytes.indexOf(10);
  const header = JSON.parse(decoded.bytes.subarray(0, headerEnd).toString('utf8'));
  if (header.version !== 4 || catalog.currentVersion !== 4) throw new Error('Native recovery requires pinned DSH format v4');
  if (typeof sessionId !== 'string' || !sessionId || header.id !== sessionId) throw new Error('Native session identity mismatch');
  const restore = catalog.createRestore(header, { recovery: 'strict', validation: 'transformed' });
  let eventCount = 0;
  for (let offset = headerEnd + 1; offset <= end;) {
    if (++eventCount > limits.maxEvents) throw limitExceeded('maxEvents');
    const next = decoded.bytes.indexOf(10, offset);
    restore.decodeRow(JSON.parse(decoded.bytes.subarray(offset, next).toString('utf8')));
    offset = next + 1;
  }
  const artifact = restore.finish();
  if (artifact.events.length > limits.maxEvents) throw limitExceeded('maxEvents');
  let hash = digest(JSON.stringify(header));
  const events = artifact.events.map((event, index) => {
    if (event.seq !== index) throw new Error('Noncontiguous native event sequence');
    hash = digest(`${hash}\n${JSON.stringify(event)}`);
    return {
      sessionId, nativeSeq: event.seq, nativeId: JSON.stringify([sessionId, event.seq]), event,
      cursor: { version: 1, sessionId, nativeSeq: event.seq, hash },
    };
  });
  return { header: artifact.header, events, fingerprint: fileFingerprint, inheritedEventCount: artifact.inheritedEventCount,
    tornBytes: decoded.tornBytes + decoded.bytes.length - end - 1 };
}

// ---------------------------------------------------------------------------
// Incremental native-log tail reader (r265).
// ---------------------------------------------------------------------------

/**
 * Per-call budgets of the incremental reader. None of them bounds the total log
 * size: a long-lived agent keeps working however long its native log grows.
 * - maxRecordBytes: largest single JSONL row / compressed frame / decoded frame.
 * - maxBytesPerScan / maxEventsPerScan / maxScanMs: work done by ONE call; the
 *   result says `more: true` and the next call continues from memory/durable state.
 * - readChunkBytes: positioned read size (memory per call is O(chunk + record)).
 * - fullVerifyBelowBytes: on a fresh controller (or a suspicious rewrite), logs
 *   below this size are re-verified from the header against the durable cursor
 *   hash; larger logs are verified by file identity + header + tail anchor.
 * - commitBatchEvents: native events per durable SQLite transaction.
 */
export const NATIVE_TAIL_LIMITS = Object.freeze({
  maxRecordBytes: 64 * 1024 * 1024,
  maxBytesPerScan: 64 * 1024 * 1024,
  maxEventsPerScan: 100000,
  maxScanMs: 500,
  readChunkBytes: 1024 * 1024,
  fullVerifyBelowBytes: 8 * 1024 * 1024,
  commitBatchEvents: 1000,
});
function tailLimits(overrides = {}) {
  if (!overrides || typeof overrides !== 'object' || Array.isArray(overrides)) throw new Error('Invalid native log limits');
  const limits = { ...NATIVE_TAIL_LIMITS, ...overrides };
  for (const [name, value] of Object.entries(limits)) {
    if (!Object.hasOwn(NATIVE_TAIL_LIMITS, name) || !Number.isSafeInteger(value) || value < (name === 'fullVerifyBelowBytes' ? 0 : 1)) throw new Error(`Invalid native log limit: ${name}`);
  }
  return limits;
}

const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const rowError = message => Object.assign(new Error(message), { code: 'DSH_NATIVE_ROW' });
const EVENT_KEYS = new Set(['type', 'seq', 'time', 'data', 'ignorable', 'sourceEventSeqs', 'surfaceOp']);
const count = value => Number.isSafeInteger(value) && value >= 0;

// Port of released v2 decodeSeqRanges (session-format-v1-to-v2/src/codec.ts):
// the logical event (and so the cursor hash) carries the EXPANDED list.
function decodeSeqRanges(value, maxEntries) {
  if (!Array.isArray(value)) throw rowError('sourceEventSeqs must be an array');
  const output = [];
  let hasRange = false;
  for (const entry of value) {
    if (!Array.isArray(entry)) {
      if (!count(entry)) throw rowError('invalid sourceEventSeqs member');
      output.push(entry);
      continue;
    }
    if (entry.length !== 2 || !count(entry[0]) || !count(entry[1])) throw rowError('sourceEventSeqs range must be a [start, end] pair');
    const [start, end] = entry;
    if (start > end || end >= maxEntries || end - start + 1 > maxEntries - output.length) throw rowError('sourceEventSeqs range exceeds its event seq');
    for (let current = start; current <= end; current += 1) output.push(current);
    hasRange = true;
  }
  const seen = new Set();
  for (const source of output) {
    if (source >= maxEntries || seen.has(source)) throw rowError('sourceEventSeqs ranges must contain unique earlier seqs');
    seen.add(source);
  }
  if (hasRange && output.some((source, index) => index > 0 && source <= output[index - 1])) throw rowError('sourceEventSeqs ranges must be strictly increasing');
  return output;
}

/**
 * Decode one committed v4 row exactly as the pinned catalog's streaming v4
 * decoder would (v4 row admission, released v2 envelope, contiguous seq).
 * Equivalence with catalog.createRestore is covered by tests.
 */
export function decodeNativeRow(value, expectedSeq, admission = null) {
  if (admission) {
    try { admission(value); } catch (error) { throw rowError(error?.message ?? String(error)); }
  }
  if (!object(value)) throw rowError('row is not an object');
  for (const key of ['type', 'seq', 'time', 'data']) if (!Object.hasOwn(value, key)) throw rowError(`row lacks required field ${key}`);
  const unexpected = Object.keys(value).find(key => !EVENT_KEYS.has(key));
  if (unexpected !== undefined) throw rowError(`row has unexpected field ${unexpected}`);
  if (typeof value.type !== 'string') throw rowError('row type must be a string');
  if (!Number.isSafeInteger(value.time)) throw rowError('row time must be a safe integer');
  if (value.ignorable !== undefined && value.ignorable !== true) throw rowError('row ignorable must be true when present');
  if (!count(value.seq)) throw rowError('row seq must be a non-negative integer');
  if (value.seq !== expectedSeq) throw rowError(`seq gap (expected ${expectedSeq}, got ${value.seq})`);
  if (value.sourceEventSeqs === undefined) return value;
  return { ...value, sourceEventSeqs: decodeSeqRanges(value.sourceEventSeqs, value.seq) };
}

/** Length of one structurally complete zstd frame at buffer start, or -1 if torn. Throws on corrupt structure. */
function zstdFrameLength(bytes) {
  let offset = 0;
  if (bytes.length < 4) return -1;
  if (bytes.readUInt32LE(0) !== 0xfd2fb528) throw new Error('Invalid Zstandard magic');
  offset += 4;
  if (offset === bytes.length) return -1;
  const descriptor = bytes[offset++];
  if (descriptor & 0x18) throw new Error('Reserved Zstandard header bit');
  const single = Boolean(descriptor & 0x20);
  const sizeFlag = descriptor >>> 6;
  const dictionary = descriptor & 3;
  const headerBytes = (single ? 0 : 1) + (dictionary === 3 ? 4 : dictionary) + (sizeFlag === 0 ? (single ? 1 : 0) : 1 << sizeFlag);
  if (bytes.length - offset < headerBytes) return -1;
  offset += headerBytes;
  for (;;) {
    if (bytes.length - offset < 3) return -1;
    const block = bytes.readUIntLE(offset, 3);
    offset += 3;
    const type = (block >>> 1) & 3;
    if (type === 3) throw new Error('Reserved Zstandard block type');
    const length = type === 1 ? 1 : block >>> 3;
    if (bytes.length - offset < length) return -1;
    offset += length;
    if (block & 1) break;
  }
  const checksumBytes = descriptor & 4 ? 4 : 0;
  if (bytes.length - offset < checksumBytes) return -1;
  return offset + checksumBytes;
}

/**
 * Physical units from `offset`: complete text rows, or every decoded row of a
 * complete zstd frame (a DSH batch, always newline-terminated). Row buffers are
 * views valid only until the consumer resumes the generator.
 */
async function* textUnits(handle, offset, limits) {
  const buffer = Buffer.allocUnsafe(limits.readChunkBytes);
  let position = offset;
  let carry = [], carryBytes = 0, carryStart = offset;
  for (;;) {
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, position);
    if (!bytesRead) { yield { kind: 'eof', pending: carryBytes }; return; }
    const chunk = buffer.subarray(0, bytesRead);
    let lineStart = 0;
    for (let newline = chunk.indexOf(10); newline !== -1; newline = chunk.indexOf(10, lineStart)) {
      const piece = chunk.subarray(lineStart, newline);
      const start = carry.length ? carryStart : position + lineStart;
      const bytes = carry.length ? Buffer.concat([...carry, piece]) : piece;
      if (bytes.length > limits.maxRecordBytes) { yield { kind: 'oversize', start }; return; }
      carry = []; carryBytes = 0;
      yield { kind: 'row', bytes, start, end: position + newline + 1, index: 0, last: true };
      lineStart = newline + 1;
    }
    if (lineStart < chunk.length) {
      if (!carry.length) carryStart = position + lineStart;
      carry.push(Buffer.from(chunk.subarray(lineStart)));
      carryBytes += chunk.length - lineStart;
      if (carryBytes > limits.maxRecordBytes) { yield { kind: 'oversize', start: carryStart }; return; }
    }
    position += bytesRead;
  }
}
async function* zstdUnits(handle, offset, limits) {
  let position = offset;
  let pending = Buffer.alloc(0);
  for (;;) {
    let length;
    try { length = zstdFrameLength(pending); }
    catch (error) { yield { kind: 'damaged', start: position, reason: `corrupt zstd frame: ${error.message}` }; return; }
    if (length < 0) {
      if (pending.length > limits.maxRecordBytes) { yield { kind: 'oversize', start: position }; return; }
      const chunk = Buffer.allocUnsafe(limits.readChunkBytes);
      const { bytesRead } = await handle.read(chunk, 0, chunk.length, position + pending.length);
      if (!bytesRead) { yield { kind: 'eof', pending: pending.length }; return; }
      pending = Buffer.concat([pending, chunk.subarray(0, bytesRead)]);
      continue;
    }
    const frame = pending.subarray(0, length);
    let decoded;
    try { decoded = zstdDecompressSync(frame, { maxOutputLength: limits.maxRecordBytes }); }
    catch (error) {
      if (error.code === 'ERR_BUFFER_TOO_LARGE') yield { kind: 'oversize', start: position };
      else yield { kind: 'damaged', start: position, reason: `corrupt zstd frame: ${error.message}` };
      return;
    }
    if (!decoded.length || decoded[decoded.length - 1] !== 10) { yield { kind: 'damaged', start: position, reason: 'zstd frame does not end with a complete record' }; return; }
    const unitHash = digest(frame);
    const start = position, end = position + length;
    const lines = [];
    for (let lineStart = 0, newline = decoded.indexOf(10); newline !== -1; lineStart = newline + 1, newline = decoded.indexOf(10, lineStart)) lines.push([lineStart, newline]);
    for (let index = 0; index < lines.length; index++) {
      yield { kind: 'row', bytes: decoded.subarray(lines[index][0], lines[index][1]), start, end, index, last: index === lines.length - 1, unitHash };
    }
    pending = pending.subarray(length);
    position = end;
  }
}
const units = (handle, offset, compression, limits) => compression === 'zstd' ? zstdUnits(handle, offset, limits) : textUnits(handle, offset, limits);

async function hashRange(handle, start, end, limits) {
  const hash = createHash('sha256');
  const buffer = Buffer.allocUnsafe(Math.min(limits.readChunkBytes, Math.max(1, end - start)));
  for (let position = start; position < end;) {
    const { bytesRead } = await handle.read(buffer, 0, Math.min(buffer.length, end - position), position);
    if (!bytesRead) return null;
    hash.update(buffer.subarray(0, bytesRead));
    position += bytesRead;
  }
  return hash.digest('hex');
}

const statusError = (state, reason, extra = {}) => ({ state, reason, ...extra });

/** Parse and admit the first record; returns the chain origin or a status. */
function admitHeader(bytes, sessionId, catalog) {
  let header;
  try { header = JSON.parse(bytes.toString('utf8')); }
  catch { return { status: statusError('damaged', 'native session header is not valid JSON', { offset: 0 }) }; }
  if (!object(header) || header.version !== 4 || catalog.currentVersion !== 4) return { status: statusError('unsupported', 'native recovery requires pinned DSH format v4') };
  if (typeof catalog.readHeader === 'function') {
    const read = catalog.readHeader(header);
    if (read.status !== 'current') return { status: statusError(read.status === 'malformed' ? 'damaged' : 'unsupported', `native header ${read.status}: ${read.reason ?? ''}`.trim(), { offset: 0 }) };
  }
  if (header.id !== sessionId) return { status: statusError('invalid', 'native session identity mismatch') };
  const hash = digest(JSON.stringify(header));
  return { hash };
}

async function readHeaderHash(handle, compression, limits, sessionId, catalog) {
  for await (const unit of units(handle, 0, compression, limits)) {
    if (unit.kind !== 'row') return unit.kind === 'eof' ? { pending: true } : { status: statusError(unit.kind === 'oversize' ? 'blocked' : 'damaged', unit.reason ?? 'native header exceeds maxRecordBytes', { offset: 0 }) };
    return admitHeader(unit.bytes, sessionId, catalog);
  }
  return { pending: true };
}

/**
 * Match DSH acp/src/updates.ts semantic ordering; never replay assistant stream
 * telemetry as well as its finalized message. Image/file blocks are attachment
 * REFERENCES (bytes live in DSH's attachment service): with a resolveImage hook
 * returning a verified ACP image block that block is used, otherwise (absent
 * resolver, resolver failure, invalid result) a text placeholder with metadata
 * is projected. Content never makes a read fail. Usage occupancy needs the live
 * tokenMeter + requestContext and is unknown from assistant.data.usage alone.
 */
export async function nativeEventUpdates(event, { resolveImage } = {}) {
  const content = async block => {
    if (!object(block)) return undefined;
    if (block.type === 'text') return typeof block.text === 'string' && block.text.length ? { type: 'text', text: block.text } : undefined;
    if (block.type === 'image' && resolveImage) {
      try {
        const image = await resolveImage(block.attachment);
        if (image?.type === 'image' && typeof image.data === 'string' && typeof image.mimeType === 'string') return image;
      } catch { /* fall through to the placeholder */ }
    }
    if (block.type === 'image' || block.type === 'file') return { type: 'text', text: attachmentPlaceholder(block), _meta: { dshAttachment: attachmentMetadata(block) } };
    if (['reasoning', 'tool-call', 'tool-addition', 'tool-removal'].includes(block.type)) return undefined;
    return { type: 'text', text: attachmentPlaceholder(block) };
  };
  const data = object(event.data) ? event.data : {};
  if (event.type === 'assistant/message') {
    const message = object(data.message) ? data.message : {};
    const updates = [];
    for (const block of nativeBlocks(message.content)) {
      const thought = block.type === 'reasoning';
      const converted = thought ? (typeof block.text === 'string' && block.text.length ? { type: 'text', text: block.text } : undefined) : await content(block);
      if (converted) updates.push({ sessionUpdate: thought ? 'agent_thought_chunk' : 'agent_message_chunk', messageId: message.id, content: converted });
    }
    return updates;
  }
  if (event.type === 'tool/call') {
    if (typeof data.callId !== 'string') return [];
    let rawInput;
    try { rawInput = JSON.parse(data.arguments); } catch { rawInput = data.arguments; }
    return [{ sessionUpdate: 'tool_call', toolCallId: data.callId, title: data.name, kind: 'other', status: 'in_progress', rawInput }];
  }
  if (event.type === 'tool/result') {
    const message = object(data.message) ? data.message : {};
    if (typeof message.toolCallId !== 'string') return [];
    const blocks = [];
    for (const block of nativeBlocks(message.content)) {
      const converted = await content(block);
      if (converted) blocks.push({ type: 'content', content: converted });
    }
    return [{ sessionUpdate: 'tool_call_update', toolCallId: message.toolCallId, status: message.isError === true ? 'failed' : 'completed', content: blocks }];
  }
  return [];
}

/**
 * In-memory companion of ONE controller lifetime (never persisted or shared).
 * It remembers the verified tail position, the last stat fingerprint and any
 * in-progress verification, so an unchanged poll costs one stat() and a changed
 * poll reads only the appended bytes. Durable resume state lives in the cursor
 * (cursor.log) inside the same SQLite transaction as the projected events.
 * maxAgeMs: an unchanged log still has its tail anchor re-hashed this often.
 */
export class NativeLogScanCache {
  constructor({ maxAgeMs = 30000 } = {}) {
    if (!Number.isSafeInteger(maxAgeMs) || maxAgeMs < 0) throw new Error('Invalid native scan cache age');
    this.maxAgeMs = maxAgeMs;
    this.entry = undefined;
  }
  clear() { this.entry = undefined; }
  get tornBytes() { return this.entry?.tornBytes ?? 0; }
  get status() { return this.entry?.status ?? null; }
}

const statFingerprint = metadata => [metadata.dev, metadata.ino, metadata.size, metadata.mtimeNs, metadata.ctimeNs].join(':');
const cursorKey = cursor => cursor ? `${cursor.nativeSeq}:${cursor.hash}` : 'none';
const validLog = (log, compression) => object(log) && log.version === 1 && log.compression === compression
  && count(log.offset) && count(log.skip) && typeof log.dev === 'string' && typeof log.ino === 'string'
  && /^[a-f0-9]{64}$/.test(log.headerHash ?? '') && object(log.anchor) && count(log.anchor.start) && count(log.anchor.end)
  && log.anchor.end > log.anchor.start && /^[a-f0-9]{64}$/.test(log.anchor.hash ?? '');

/**
 * Runtime/store boundary:
 *   store.nativeCursor(sessionId) -> persisted cursor | null
 *   store.commitNative({ previousCursor, cursor, nativeId, nativeSeq, sessionId,
 *                        event, updates }) -> Promise<void>
 *   optional store.commitNativeBatch([...records]) -> one transaction for many
 *   optional store.nativeStatus(sessionId) / store.recordNativeStatus(sessionId, status)
 * commitNative MUST atomically CAS previousCursor, insert the unique
 * (sessionId,nativeSeq) raw event, append ALL its projections, and advance cursor
 * in one durable transaction. Reject stale CAS; never silently accept it.
 * Emit notifications only after that transaction commits. Re-run after failure.
 * Empty-update events must advance cursor too. Turn/user metadata is in event;
 * the integrator binds native data.turn to its durable turn ID in that txn.
 *
 * Make this the authoritative path for both normal delivery and startup replay.
 * ACP notifications have no native seq; use them only to schedule a scan.
 * Serialize scans per session, after resume and before marking a turn interrupted.
 *
 * Incremental tailing (r265): cursor = { version: 1, sessionId, nativeSeq, hash,
 * log: { version: 1, compression, dev, ino, offset, skip, headerHash, anchor } }.
 * `offset`/`skip` is the physical resume point after nativeSeq (zstd: frame
 * start + decoded rows already consumed). The hash chain is unchanged from r264,
 * so r264 cursors (without `log`) are upgraded by one streaming verification.
 * A resume point is trusted only if the file identity (dev, ino) matches, the
 * file has not shrunk below it, the header hash matches and the anchor (raw bytes
 * of the last committed row/frame) re-hashes identically; logs smaller than
 * fullVerifyBelowBytes are re-verified from the header on every fresh start.
 * Otherwise (rotation, copy, truncation, rewrite, unknown cursor) the log is
 * streamed from its header WITHOUT committing until the durable cursor hash is
 * reproduced; then tailing continues. Mismatch => status `diverged`; log ends
 * before the cursor => status `behind` (both keep polling cheaply, never throw).
 *
 * Damage: a complete but invalid row/frame stops the scan at the last good
 * record (status `damaged`, with offset/reason); everything before it is
 * committed. DSH's own recoverable scanner truncates such a tail on its next
 * open and appends from there, which this reader then follows. An unterminated
 * final row / torn final frame is normal write-in-progress (tornBytes > 0).
 * A record above maxRecordBytes => status `blocked`. Log-content problems are
 * statuses, never exceptions; only caller errors, I/O errors and store errors
 * (e.g. a failed commit, which must be retried) reject.
 *
 * Each call is bounded by NATIVE_TAIL_LIMITS and returns `more: true` if it
 * stopped early; callers that need the log fully applied (startup, before
 * prompting) loop while `more`. Memory per call is O(readChunkBytes +
 * maxRecordBytes + commitBatchEvents), independent of log length.
 */
export async function reconcileNativeLog({ store, resolveImage, cache, ...options }) {
  const limits = tailLimits(options.limits);
  const catalog = options.catalog ?? await loadNativeCatalog();
  const admission = rowAdmissions.get(catalog) ?? null;
  if (cache !== undefined && !(cache instanceof NativeLogScanCache)) throw new Error('Invalid native scan cache');
  const { logPath, sessionId } = options;
  if (typeof logPath !== 'string' || !logPath) throw new Error('Native log path required');
  if (typeof sessionId !== 'string' || !sessionId) throw new Error('Native session ID must be nonempty');
  const compression = options.compression ?? (logPath.endsWith('.zstd') ? 'zstd' : 'none');
  if (!['none', 'zstd'].includes(compression)) throw new Error('Unknown native log compression');
  const started = performance.now();
  const durable = await store.nativeCursor(sessionId) ?? null;
  if (durable && (durable.version !== 1 || durable.sessionId !== sessionId || !count(durable.nativeSeq) || !/^[a-f0-9]{64}$/.test(durable.hash ?? ''))) {
    throw new Error('Native cursor does not match durable log prefix');
  }
  const key = JSON.stringify([resolve(logPath), sessionId, compression, limits]);
  let entry = cache?.entry;
  if (!entry || entry.key !== key || entry.store !== store || entry.catalog !== catalog) entry = { key, store, catalog, status: undefined, statusKey: undefined };

  let fingerprint;
  const finish = async (result, status) => {
    entry.stat = fingerprint;
    entry.cursorKey = cursorKey(result.cursor);
    entry.more = Boolean(result.more);
    status = { ...status, nativeSeq: result.cursor?.nativeSeq ?? -1, observedAt: new Date().toISOString() };
    const statusKey = JSON.stringify([status.state, status.reason ?? null, status.offset ?? null]);
    if (entry.statusKey === undefined && typeof store.nativeStatus === 'function') {
      const saved = await store.nativeStatus(sessionId);
      entry.statusKey = saved ? JSON.stringify([saved.state, saved.reason ?? null, saved.offset ?? null]) : undefined;
    }
    if (statusKey !== entry.statusKey && typeof store.recordNativeStatus === 'function') await store.recordNativeStatus(sessionId, status);
    entry.statusKey = statusKey;
    entry.status = status;
    if (cache) cache.entry = entry;
    return { tornBytes: 0, more: false, unchanged: false, committed: 0, updateCount: 0, ...result, status };
  };

  let handle;
  try { handle = await open(logPath, 'r'); }
  catch (error) {
    if (error.code !== 'ENOENT') throw error;
    entry.tail = undefined; entry.verify = undefined; entry.stat = undefined;
    return finish({ cursor: durable }, statusError('pending', 'native log not materialized'));
  }
  try {
    const metadata = await handle.stat({ bigint: true });
    if (!metadata.isFile()) { entry.tail = undefined; entry.verify = undefined; return finish({ cursor: durable }, statusError('invalid', 'native log must be a regular file')); }
    const size = Number(metadata.size);
    const dev = String(metadata.dev), ino = String(metadata.ino);
    fingerprint = statFingerprint(metadata);

    // Fast path: same file, same bytes, same durable cursor, nothing pending.
    const sameCursor = entry.cursorKey === cursorKey(durable);
    if (sameCursor && entry.stat === fingerprint && !entry.more) {
      const anchorDue = entry.tail?.anchor && performance.now() - (entry.anchorCheckedAt ?? 0) >= (cache?.maxAgeMs ?? 0);
      if (!anchorDue || await hashRange(handle, entry.tail.anchor.start, entry.tail.anchor.end, limits) === entry.tail.anchor.hash) {
        if (anchorDue) entry.anchorCheckedAt = performance.now();
        return { cursor: durable, committed: 0, updateCount: 0, tornBytes: entry.tornBytes ?? 0, unchanged: true, more: false, status: entry.status };
      }
      entry.tail = undefined;
    }

    // Establish where to read from: a live tail (trusted), or a verification pass.
    let plan;
    const tail = sameCursor ? entry.tail : undefined;
    const grewOnly = entry.stat && tail && tail.dev === dev && tail.ino === ino && size >= tail.anchor.end
      && size > Number(entry.stat.split(':')[2]);
    // Pure growth of the same file is an append. Any other change (same-size
    // rewrite, shrink) re-hashes the anchor, or re-verifies a small log fully.
    if (tail && tail.dev === dev && tail.ino === ino && size >= tail.anchor.end
      && (entry.stat === fingerprint || grewOnly || (size >= limits.fullVerifyBelowBytes && await hashRange(handle, tail.anchor.start, tail.anchor.end, limits) === tail.anchor.hash))) {
      plan = { mode: 'tail', offset: tail.offset, skip: tail.skip, nextSeq: tail.nextSeq, hash: tail.hash, headerHash: tail.headerHash, anchor: tail.anchor };
    } else if (sameCursor && entry.verify && entry.verify.dev === dev && entry.verify.ino === ino && size >= entry.verify.offset) {
      plan = { ...entry.verify };
    } else if (!durable) {
      plan = { mode: 'tail', offset: 0, skip: 0, nextSeq: 0, hash: undefined, headerHash: undefined };
    } else {
      const log = durable.log;
      let trusted = false;
      if (validLog(log, compression) && log.dev === dev && log.ino === ino && size >= Math.max(log.offset, log.anchor.end)
        && size >= limits.fullVerifyBelowBytes) {
        const header = await readHeaderHash(handle, compression, limits, sessionId, catalog);
        if (header.status) { entry.tail = undefined; entry.verify = undefined; return finish({ cursor: durable }, header.status); }
        trusted = header.hash === log.headerHash && await hashRange(handle, log.anchor.start, log.anchor.end, limits) === log.anchor.hash;
      }
      plan = trusted
        ? { mode: 'tail', offset: log.offset, skip: log.skip, nextSeq: durable.nativeSeq + 1, hash: durable.hash, headerHash: log.headerHash, anchor: log.anchor }
        : { mode: 'verify', offset: 0, skip: 0, nextSeq: 0, hash: undefined, headerHash: undefined, target: { nativeSeq: durable.nativeSeq, hash: durable.hash } };
    }
    entry.tail = undefined; entry.verify = undefined;

    let { mode, offset, skip, nextSeq, hash, headerHash, anchor } = plan;
    const target = plan.target;
    let previous = durable;
    let committed = 0, updateCount = 0, rows = 0, tornBytes = 0, more = false, status = statusError('ok');
    let batch = [];
    const flush = async () => {
      if (!batch.length) return;
      const records = batch;
      batch = [];
      if (typeof store.commitNativeBatch === 'function') await store.commitNativeBatch(records);
      else for (const record of records) await store.commitNative(record);
      committed += records.length;
    };
    const firstUnit = offset;
    let budgetStart = started, budgetOffset = offset;
    let skipRemaining = skip;
    try {
      for await (const unit of units(handle, offset, compression, limits)) {
        if (unit.kind === 'eof') { tornBytes = unit.pending; break; }
        if (unit.kind === 'oversize') { status = statusError('blocked', `native record exceeds maxRecordBytes (${limits.maxRecordBytes})`, { offset: unit.start }); break; }
        if (unit.kind === 'damaged') { status = statusError('damaged', unit.reason, { offset: unit.start }); break; }
        if (unit.start === firstUnit && skipRemaining > 0) { skipRemaining--; continue; }
        if (unit.start === 0 && unit.index === 0) {
          const header = admitHeader(unit.bytes, sessionId, catalog);
          if (header.status) { status = header.status; break; }
          hash = headerHash = header.hash;
        } else {
          if (headerHash === undefined) { status = statusError('damaged', 'native log has no header', { offset: unit.start }); break; }
          let event;
          try { event = decodeNativeRow(JSON.parse(unit.bytes.toString('utf8')), nextSeq, admission); }
          catch (error) {
            status = statusError('damaged', `invalid committed native record at seq ${nextSeq}: ${error.code === 'DSH_NATIVE_ROW' ? error.message : 'unparsable JSON'}`, { offset: unit.start });
            break;
          }
          hash = digest(`${hash}\n${JSON.stringify(event)}`);
          nextSeq++;
          rows++;
          const resume = compression === 'zstd' && !unit.last ? { offset: unit.start, skip: unit.index + 1 } : { offset: unit.end, skip: 0 };
          anchor = { start: unit.start, end: unit.end, hash: compression === 'zstd' ? unit.unitHash : createHash('sha256').update(unit.bytes).update('\n').digest('hex') };
          if (mode === 'verify') {
            if (event.seq === target.nativeSeq) {
              if (hash !== target.hash) { status = statusError('diverged', `native log differs from projected history at seq ${event.seq}`, { offset: unit.start }); break; }
              mode = 'tail';
              // Without a cache the verification cannot be resumed: budget only the tail part.
              if (!cache) { rows = 0; budgetStart = performance.now(); budgetOffset = unit.end; }
            }
          } else {
            const cursor = { version: 1, sessionId, nativeSeq: event.seq, hash,
              log: { version: 1, compression, dev, ino, ...resume, headerHash, anchor } };
            const updates = await nativeEventUpdates(event, { resolveImage });
            batch.push({ sessionId, nativeSeq: event.seq, nativeId: JSON.stringify([sessionId, event.seq]), event, cursor, previousCursor: previous, updates });
            previous = cursor;
            updateCount += updates.length;
            if (batch.length >= limits.commitBatchEvents) await flush();
          }
          offset = resume.offset; skip = resume.skip;
        }
        if (unit.start === 0 && unit.index === 0) { offset = compression === 'zstd' && !unit.last ? 0 : unit.end; skip = compression === 'zstd' && !unit.last ? unit.index + 1 : 0; }
        // Verification progress survives only in a cache; without one, verify to the end.
        if ((mode === 'tail' || cache) && (rows >= limits.maxEventsPerScan || offset - budgetOffset >= limits.maxBytesPerScan
          || performance.now() - budgetStart >= limits.maxScanMs)) { more = true; break; }
      }
      await flush();
    } catch (error) {
      // Durable state is authoritative after a failed commit; rebuild from it.
      if (cache) { entry.tail = undefined; entry.verify = undefined; entry.stat = undefined; entry.cursorKey = undefined; cache.entry = entry; }
      throw error;
    }
    const cursor = previous;
    entry.tornBytes = tornBytes;
    entry.anchorCheckedAt = performance.now();
    if (status.state === 'ok' || status.state === 'blocked' || status.state === 'damaged') {
      if (mode === 'tail' && headerHash !== undefined) entry.tail = { dev, ino, offset, skip, nextSeq, hash, headerHash, anchor: anchor ?? { start: 0, end: Math.max(1, offset), hash: await hashRange(handle, 0, Math.max(1, offset), limits) } };
      else if (mode === 'verify' && status.state === 'ok') {
        entry.verify = { mode, dev, ino, offset, skip, nextSeq, hash, headerHash, target };
        if (!more) status = statusError('behind', `native log ends at seq ${nextSeq - 1}, before projected seq ${target.nativeSeq}`);
      }
    }
    if (status.state === 'ok' && headerHash === undefined) status = statusError('pending', 'native header not yet written');
    if (status.state === 'ok' && more) status = statusError('catchingUp', mode === 'verify' ? 'verifying native log prefix' : 'applying native log');
    return finish({ cursor, committed, updateCount, tornBytes, more }, status);
  } finally { await handle.close(); }
}

// ---------------------------------------------------------------------------
// Native session delete (r265). DSH 0.2.1-rc.3 has no delete in ACP or in its
// JSONL persistence; a session is exactly its directory
// <root>/<project>/<encoded-id>/ (session[.vN].jsonl[.zstd] + session.lock).
// Attachments are content-addressed under DSH_HOME/attachments and shared, so
// they are left alone; spill files are per-process temporaries.
// ---------------------------------------------------------------------------

const activeError = path => Object.assign(new Error(`Native DSH session is still owned by a running writer: ${path}`), { code: 'DSH_NATIVE_SESSION_ACTIVE' });

/** Linux only: whether any process holds a flock on this inode (DSH's write lease). */
async function leaseHeld(lockPath) {
  if (process.platform !== 'linux') return false;
  let metadata;
  try { metadata = await lstat(lockPath, { bigint: true }); }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
  let locks;
  try { locks = await (await import('node:fs/promises')).readFile('/proc/locks', 'utf8'); }
  catch { return false; }
  const inode = String(metadata.ino);
  return locks.split('\n').some(line => {
    const fields = line.trim().split(/\s+/);
    const id = fields.find(field => /^[0-9a-f]+:[0-9a-f]+:\d+$/i.test(field));
    return fields.includes('FLOCK') && id?.split(':')[2] === inode;
  });
}

/** Every session directory under an explicit root, without following links. */
async function sessionDirectories(root) {
  const list = async path => {
    try { return await readdir(path, { withFileTypes: true }); }
    catch (error) { if (error.code === 'ENOENT') return []; throw error; }
  };
  const result = [];
  for (const project of await list(root)) {
    if (!project.isDirectory() || project.isSymbolicLink()) continue;
    for (const entry of await list(join(root, project.name))) {
      if (entry.isDirectory() && !entry.isSymbolicLink()) result.push({ project: project.name, name: entry.name, path: join(root, project.name, entry.name) });
    }
  }
  return result;
}

/** Parsed header of the highest canonical generation in a session directory, or null. */
async function directoryHeader(directory) {
  const generations = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const match = /^session(?:\.v([1-9][0-9]*))?\.jsonl(\.zstd)?$/.exec(entry.name);
    if (match && entry.isFile()) generations.push({ path: join(directory, entry.name), version: Number(match[1] ?? 0), compression: match[2] ? 'zstd' : 'none' });
  }
  const latest = generations.sort((a, b) => b.version - a.version)[0];
  if (!latest) return null;
  const handle = await open(latest.path, 'r');
  try {
    for await (const unit of units(handle, 0, latest.compression, { ...NATIVE_TAIL_LIMITS, maxRecordBytes: 1024 * 1024, readChunkBytes: 64 * 1024 })) {
      if (unit.kind !== 'row') return null;
      try { return JSON.parse(unit.bytes.toString('utf8')); } catch { return null; }
    }
    return null;
  } finally { await handle.close(); }
}

/**
 * Delete one native DSH session (and, by default, its subagent child sessions,
 * i.e. sessions whose header.parentSession chains to it) from an explicit
 * persistence root, plus optionally the adapter's timeline database.
 *
 * Contract: the caller has stopped the agent's adapter/DSH process. As a second
 * guard on Linux, a session whose session.lock is flock-held is refused with
 * DSH_NATIVE_SESSION_ACTIVE before anything is touched. Each session directory
 * is first renamed out of the root (atomic: DSH never lists a half-deleted
 * session), then removed. Symlinked directories are never followed. Missing
 * sessions are not an error (idempotent). Returns the removed paths.
 */
export async function deleteNativeSession({ sessionsRoot, sessionId, database, includeChildren = true, recoveryDirectory } = {}) {
  if (typeof sessionsRoot !== 'string' || !isAbsolute(sessionsRoot)) throw new Error('Explicit absolute native persistence root required');
  if (database !== undefined && (typeof database !== 'string' || !isAbsolute(database))) throw new Error('database must be an absolute path');
  if (recoveryDirectory !== undefined && (typeof recoveryDirectory !== 'string' || !isAbsolute(recoveryDirectory))) throw new Error('recoveryDirectory must be an absolute path');
  const root = resolve(sessionsRoot);
  const encoded = encodeSegment(sessionId);
  const directories = await sessionDirectories(root);
  const targets = directories.filter(entry => entry.name === encoded);
  if (targets.length > 1) throw new Error('Duplicate native session across project directories');
  if (includeChildren) {
    const headers = new Map();
    for (const entry of directories) headers.set(entry.path, await directoryHeader(entry.path).catch(() => null));
    const ids = new Set([sessionId]);
    for (let grew = true; grew;) {
      grew = false;
      for (const entry of directories) {
        const header = headers.get(entry.path);
        if (header && typeof header.id === 'string' && !ids.has(header.id) && ids.has(header.parentSession)) {
          ids.add(header.id); targets.push(entry); grew = true;
        }
      }
    }
  }
  for (const target of targets) {
    const metadata = await lstat(target.path);
    if (metadata.isSymbolicLink() || !metadata.isDirectory()) throw new Error('Native session directory must not be a symlink');
    if (await leaseHeld(join(target.path, 'session.lock'))) throw activeError(target.path);
  }
  const { rename, rm, mkdir, rmdir, cp } = await import('node:fs/promises');
  const removed = [];
  // Move instead of delete (cutex's recovery-copy semantics for Codex/Claude deletes).
  const move = async (from, to) => {
    try { await rename(from, to); }
    catch (error) {
      if (error.code !== 'EXDEV') throw error;
      await cp(from, to, { recursive: true, errorOnExist: true, force: false, verbatimSymlinks: true });
      await rm(from, { recursive: true, force: true });
    }
  };
  if (recoveryDirectory !== undefined) {
    await mkdir(join(recoveryDirectory, 'dsh-sessions'), { recursive: true, mode: 0o700 });
    for (const target of targets) {
      const destination = join(recoveryDirectory, 'dsh-sessions', target.project, target.name);
      await mkdir(dirname(destination), { recursive: true, mode: 0o700 });
      try { await move(target.path, destination); removed.push(target.path); }
      catch (error) { if (error.code !== 'ENOENT') throw error; }
    }
    if (database !== undefined) {
      for (const path of [database, `${database}-wal`, `${database}-shm`]) {
        try { await move(path, join(recoveryDirectory, basename(path))); removed.push(path); }
        catch (error) { if (error.code !== 'ENOENT') throw error; }
      }
    }
    return { removed, recoveryDirectory };
  }
  if (targets.length) {
    const trash = `${root}.cutex-deleted`;
    await mkdir(trash, { recursive: true, mode: 0o700 });
    for (const target of targets) {
      const tombstone = join(trash, `${target.name.slice(0, 120)}-${createHash('sha256').update(`${target.path}:${process.hrtime.bigint()}`).digest('hex').slice(0, 16)}`);
      try { await rename(target.path, tombstone); }
      catch (error) { if (error.code === 'ENOENT') continue; throw error; }
      await rm(tombstone, { recursive: true, force: true });
      removed.push(target.path);
    }
    try { await rmdir(trash); } catch { /* another delete may be in progress */ }
  }
  if (database !== undefined) {
    for (const path of [database, `${database}-wal`, `${database}-shm`]) {
      try { await rm(path); removed.push(path); }
      catch (error) { if (error.code !== 'ENOENT') throw error; }
    }
  }
  return { removed };
}

// ---------------------------------------------------------------------------
// EXPERIMENTAL building block for fork / revert (r265 investigation; not wired).
// DSH 0.2.1-rc.3 forks only through its HTTP session controller, and its ACP
// session/resume refuses seeded children (header.parentSession). An ordinary,
// unseeded session whose log is an exact closed prefix of another session's
// log IS resumable over ACP: physical rows are copied byte-for-byte (same seq
// numbers), only the header is new.
// ---------------------------------------------------------------------------

/**
 * Write a new native session `newSessionId` whose history is the source
 * session's events 0..throughSeq (inclusive; must end a completed turn, or -1
 * for an empty history). The source must be a top-level session. The new log is
 * created atomically in the source's project directory (under
 * targetSessionsRoot, default the source root) and is never overwritten.
 * Attachments (DSH_HOME/attachments) referenced by the prefix are NOT copied. Returns { logPath, sessionDirectory, events }.
 */
export async function writeNativeSessionCopy({ sessionsRoot, sourceSessionId, newSessionId, throughSeq, catalog, createdAt = Date.now(), targetSessionsRoot = sessionsRoot } = {}) {
  if (typeof targetSessionsRoot !== 'string' || !isAbsolute(targetSessionsRoot)) throw new Error('Explicit absolute target persistence root required');
  catalog ??= await loadNativeCatalog();
  if (!Number.isSafeInteger(throughSeq) || throughSeq < -1) throw new Error('throughSeq must be -1 or a native event seq');
  if (typeof newSessionId !== 'string' || !newSessionId || newSessionId === sourceSessionId) throw new Error('new native session ID required');
  const source = await locateNativeLog(sessionsRoot, sourceSessionId);
  if (!source) throw new Error('source native session not found');
  if (await locateNativeLog(targetSessionsRoot, newSessionId)) throw new Error('target native session already exists');
  const compression = source.endsWith('.zstd') ? 'zstd' : 'none';
  const admission = rowAdmissions.get(catalog) ?? null;
  const rows = [];
  let header, last, nextSeq = 0;
  const handle = await open(source, 'r');
  try {
    for await (const unit of units(handle, 0, compression, NATIVE_TAIL_LIMITS)) {
      if (unit.kind !== 'row') break;
      if (unit.start === 0 && unit.index === 0) { header = JSON.parse(unit.bytes.toString('utf8')); continue; }
      if (nextSeq > throughSeq) break;
      const event = decodeNativeRow(JSON.parse(unit.bytes.toString('utf8')), nextSeq++, admission);
      rows.push(Buffer.from(unit.bytes));
      last = event;
    }
  } finally { await handle.close(); }
  if (!object(header) || header.version !== 4 || header.id !== sourceSessionId) throw new Error('source native header is not a pinned v4 header for this session');
  if (header.parentSession !== undefined || header.origin !== undefined) throw new Error('only top-level native sessions can be copied');
  if (rows.length !== throughSeq + 1) throw new Error(`source native log ends before seq ${throughSeq}`);
  if (throughSeq >= 0 && last.type !== 'turn/end') throw new Error('copy boundary must be the turn/end of a completed turn');
  const newHeader = { version: 4, id: newSessionId, createdAt, ...(header.cwd === undefined ? {} : { cwd: header.cwd }), isSeeded: false, delegationDepth: header.delegationDepth ?? 0,
    ...(header.agentPreset === undefined ? {} : { agentPreset: header.agentPreset }) };
  const body = Buffer.concat([Buffer.from(JSON.stringify(catalog.encodeCurrentHeader(newHeader, 0)) + '\n'), ...rows.flatMap(row => [row, Buffer.from('\n')])]);
  const { mkdir, writeFile, rename, rm } = await import('node:fs/promises');
  const { zstdCompressSync, constants } = await import('node:zlib');
  // Same project directory name (DSH derives it from header.cwd), possibly under another root (fork to a new agent home).
  const projectDirectory = join(resolve(targetSessionsRoot), basename(dirname(dirname(source))));
  await mkdir(projectDirectory, { recursive: true, mode: 0o700 });
  const sessionDirectory = join(projectDirectory, encodeSegment(newSessionId));
  await mkdir(sessionDirectory, { mode: 0o700 });
  const logPath = join(sessionDirectory, basename(source));
  const temporary = join(sessionDirectory, `.${basename(source)}.cutex-tmp`);
  try {
    await writeFile(temporary, compression === 'zstd' ? zstdCompressSync(body, { params: { [constants.ZSTD_c_checksumFlag]: 1 } }) : body, { flag: 'wx', mode: 0o600, flush: true });
    await rename(temporary, logPath);
  } catch (error) { await rm(sessionDirectory, { recursive: true, force: true }); throw error; }
  return { logPath, sessionDirectory, events: rows.length };
}
