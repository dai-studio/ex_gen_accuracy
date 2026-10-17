import { randomBytes } from 'node:crypto';
import type { IncomingMessage, OutgoingHttpHeaders, ServerResponse } from 'node:http';

/** An error that maps directly onto an HTTP error response. */
export class HttpError extends Error {
  readonly status: number;
  readonly code: string;
  readonly headers: OutgoingHttpHeaders;

  constructor(status: number, code: string, message: string, headers: OutgoingHttpHeaders = {}) {
    super(message);
    this.name = 'HttpError';
    this.status = status;
    this.code = code;
    this.headers = headers;
  }
}

/** OpenAI-style error envelope, so SDK clients surface the message nicely. */
export function errorBody(message: string, code: string, type = 'moxy_error'): { error: { message: string; type: string; code: string } } {
  return { error: { message, type, code } };
}

const DURATION_UNITS: Record<string, number> = { ms: 1, s: 1000, m: 60_000, h: 3_600_000, d: 86_400_000 };

/** Parses "250ms", "30s", "5m", "1h", "1d". A bare number means seconds. */
export function parseDuration(value: string): number {
  const m = /^\s*(\d+(?:\.\d+)?)\s*(ms|s|m|h|d)?\s*$/i.exec(value);
  if (!m) throw new Error(`Invalid duration "${value}" (examples: 500ms, 30s, 5m, 1h, 1d)`);
  return Math.round(parseFloat(m[1]!) * DURATION_UNITS[(m[2] ?? 's').toLowerCase()]!);
}

const SIZE_UNITS: Record<string, number> = { '': 1, b: 1, k: 1024, kb: 1024, m: 1024 ** 2, mb: 1024 ** 2, g: 1024 ** 3, gb: 1024 ** 3 };

/** Parses "512", "64kb", "10mb", "1g" (1024-based). A bare number means bytes. */
export function parseSize(value: string): number {
  const m = /^\s*(\d+(?:\.\d+)?)\s*([kmg]?b?)\s*$/i.exec(value);
  if (!m) throw new Error(`Invalid size "${value}" (examples: 512kb, 10mb)`);
  return Math.round(parseFloat(m[1]!) * SIZE_UNITS[m[2]!.toLowerCase()]!);
}

export function parseBool(value: string | undefined, fallback: boolean): boolean {
  if (value === undefined || value.trim() === '') return fallback;
  const v = value.trim().toLowerCase();
  if (['true', 'yes', 'on', '1'].includes(v)) return true;
  if (['false', 'no', 'off', '0'].includes(v)) return false;
  throw new Error(`Invalid boolean "${value}" (use true or false)`);
}

export function parseIntStrict(value: string, what: string, min = 0): number {
  if (!/^\s*\d+\s*$/.test(value)) throw new Error(`Invalid ${what} "${value}" (expected a whole number)`);
  const n = parseInt(value, 10);
  if (n < min) throw new Error(`Invalid ${what} "${value}" (must be at least ${min})`);
  return n;
}

/** Splits a comma- or whitespace-separated list, dropping empty entries. */
export function splitList(value: string | undefined): string[] {
  if (!value) return [];
  return value
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** Converts a simple glob ("gpt-4*", "*-mini") into an anchored RegExp. */
export function globToRegExp(glob: string): RegExp {
  const escaped = glob.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.');
  return new RegExp(`^${escaped}$`);
}

export function matchesAny(value: string, patterns: RegExp[]): boolean {
  return patterns.some((p) => p.test(value));
}

function pathSegments(path: string): string[] {
  return path.split('.').filter((s) => s.length > 0);
}

/** Reads a dot-separated path ("a.b.0.c") from a JSON value. */
export function getJsonPath(obj: unknown, path: string): unknown {
  let cur: unknown = obj;
  for (const seg of pathSegments(path)) {
    if (cur === null || typeof cur !== 'object') return undefined;
    cur = (cur as Record<string, unknown>)[seg];
  }
  return cur;
}

/** Writes a dot-separated path, creating intermediate objects as needed. */
export function setJsonPath(obj: Record<string, unknown>, path: string, value: unknown): void {
  const segs = pathSegments(path);
  if (segs.length === 0) throw new Error('Empty JSON path');
  if (segs.some((s) => s === '__proto__' || s === 'constructor' || s === 'prototype')) {
    throw new Error(`Refusing unsafe JSON path "${path}"`);
  }
  let cur: Record<string, unknown> = obj;
  for (const seg of segs.slice(0, -1)) {
    const next = cur[seg];
    if (next === null || typeof next !== 'object') {
      const created: Record<string, unknown> = {};
      cur[seg] = created;
      cur = created;
    } else {
      cur = next as Record<string, unknown>;
    }
  }
  cur[segs[segs.length - 1]!] = value;
}

export function parseCookies(header: string | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  if (!header) return out;
  for (const part of header.split(';')) {
    const eq = part.indexOf('=');
    if (eq < 0) continue;
    const name = part.slice(0, eq).trim();
    if (!name || name === '__proto__') continue;
    let value = part.slice(eq + 1).trim();
    if (value.startsWith('"') && value.endsWith('"')) value = value.slice(1, -1);
    try {
      out[name] = decodeURIComponent(value);
    } catch {
      out[name] = value;
    }
  }
  return out;
}

/** Removes one cookie from a Cookie header, returning undefined if nothing is left. */
export function removeCookie(header: string | undefined, name: string): string | undefined {
  if (!header) return undefined;
  const kept = header
    .split(';')
    .map((p) => p.trim())
    .filter((p) => p && p.split('=')[0]!.trim() !== name);
  return kept.length ? kept.join('; ') : undefined;
}

/** Buffers a request body, failing with 413 once it exceeds `limit` bytes. */
export function readBody(req: IncomingMessage, limit: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const declared = Number(req.headers['content-length']);
    if (Number.isFinite(declared) && declared > limit) {
      req.resume();
      reject(new HttpError(413, 'payload_too_large', `Request body exceeds ${limit} bytes`));
      return;
    }
    const chunks: Buffer[] = [];
    let size = 0;
    let failed = false;
    req.on('data', (chunk: Buffer) => {
      if (failed) return;
      size += chunk.length;
      if (size > limit) {
        failed = true;
        reject(new HttpError(413, 'payload_too_large', `Request body exceeds ${limit} bytes`));
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => {
      if (!failed) resolve(Buffer.concat(chunks));
    });
    req.on('error', (err) => {
      if (!failed) reject(err);
    });
  });
}

export function randomId(bytes = 12): string {
  return randomBytes(bytes).toString('base64url');
}

export function sendJson(res: ServerResponse, status: number, body: unknown, headers: OutgoingHttpHeaders = {}): void {
  const payload = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': payload.length,
    'cache-control': 'no-store',
    ...headers,
  });
  res.end(payload);
}

export function sendError(res: ServerResponse, status: number, code: string, message: string, headers: OutgoingHttpHeaders = {}): void {
  sendJson(res, status, errorBody(message, code), headers);
}

export function redirect(res: ServerResponse, location: string, status = 302): void {
  res.writeHead(status, { location, 'content-length': 0, 'cache-control': 'no-store' });
  res.end();
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);
}

/** Only allows same-site relative redirect targets ("/chat", not "//evil" or "https://..."). */
export function safeNextPath(next: string | null | undefined, fallback: string): string {
  if (!next || !next.startsWith('/') || next.startsWith('//') || next.startsWith('/\\')) return fallback;
  return next;
}

export function headerValue(value: string | string[] | undefined): string | undefined {
  if (Array.isArray(value)) return value.join(', ');
  return value;
}
