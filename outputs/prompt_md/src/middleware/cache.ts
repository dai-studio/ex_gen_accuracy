import { createHash } from 'node:crypto';
import type { OutgoingHttpHeaders } from 'node:http';
import { parseBool, parseDuration, parseIntStrict, splitList } from '../util.ts';
import { attrs, parsed } from './helpers.ts';
import type { MiddlewareFactory } from './types.ts';

interface Entry {
  status: number;
  headers: OutgoingHttpHeaders;
  body: Buffer;
  storedAt: number;
  expiresAt: number;
}

const UNCACHED_HEADERS = /^(set-cookie|content-length|transfer-encoding|connection|date|x-request-id|x-upstream-request-id|x-ratelimit-.*|x-moxy-.*|retry-after)$/i;

/**
 * <cache ttl="5m" max-entries="500" per-principal="true" methods="POST"/>
 * Caches successful non-streaming responses keyed by route, path and body,
 * so identical prompts are answered without another upstream call.
 */
export const cache: MiddlewareFactory = (def) => {
  const a = attrs(def, ['ttl', 'max-entries', 'per-principal', 'methods']);
  const ttlMs = parsed(def, () => parseDuration(a.ttl ?? '5m'));
  const maxEntries = parsed(def, () => parseIntStrict(a['max-entries'] ?? '500', 'max-entries', 1));
  const perPrincipal = parsed(def, () => parseBool(a['per-principal'], true));
  const methods = splitList(a.methods ?? 'GET,POST').map((m) => m.toUpperCase());
  // Map iteration order doubles as LRU order: hits are re-inserted at the end.
  const entries = new Map<string, Entry>();

  return {
    type: 'cache',
    bufferResponse: true,
    onRequest(ctx) {
      if (!methods.includes(ctx.method)) return;
      if (ctx.getJson()?.stream === true) return;
      if (/no-cache|no-store/i.test(ctx.headers['cache-control'] ?? '')) return;
      const hash = createHash('sha256');
      for (const part of [ctx.route.id, ctx.method, ctx.upstreamPath, ctx.query, perPrincipal ? (ctx.principal?.id ?? `ip:${ctx.clientIp}`) : '']) {
        hash.update(part).update('\0');
      }
      hash.update(ctx.finalBody());
      const key = hash.digest('hex');
      const now = Date.now();
      const hit = entries.get(key);
      if (hit && hit.expiresAt > now) {
        entries.delete(key);
        entries.set(key, hit);
        return {
          status: hit.status,
          headers: { ...hit.headers, 'x-moxy-cache': 'HIT', age: String(Math.floor((now - hit.storedAt) / 1000)) },
          body: hit.body,
        };
      }
      if (hit) entries.delete(key);
      ctx.state.set('cache.key', key);
    },
    onResponse(ctx, res) {
      const key = ctx.state.get('cache.key') as string | undefined;
      if (!key) return;
      res.headers['x-moxy-cache'] = 'MISS';
      if (res.source !== 'upstream' || res.status !== 200 || !res.body) return;
      if (/no-store|private/i.test(String(res.headers['cache-control'] ?? ''))) return;
      const headers: OutgoingHttpHeaders = {};
      for (const [k, v] of Object.entries(res.headers)) if (!UNCACHED_HEADERS.test(k) && v !== undefined) headers[k] = v;
      const now = Date.now();
      entries.set(key, { status: res.status, headers, body: res.body, storedAt: now, expiresAt: now + ttlMs });
      while (entries.size > maxEntries) entries.delete(entries.keys().next().value!);
    },
  };
};
