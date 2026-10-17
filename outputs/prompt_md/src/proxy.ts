import http, { type ClientRequest, type IncomingHttpHeaders, type IncomingMessage, type OutgoingHttpHeaders } from 'node:http';
import https from 'node:https';
import type { RouteDef, UpstreamDef } from './config.ts';
import type { Context } from './context.ts';
import type { CompiledRoute } from './middleware/types.ts';
import { removeCookie } from './util.ts';

const DEFAULT_TIMEOUT_MS = 120_000;

/** Request headers that must not be forwarded as-is. */
const HOP_BY_HOP = new Set([
  'connection',
  'keep-alive',
  'proxy-authenticate',
  'proxy-authorization',
  'proxy-connection',
  'te',
  'trailer',
  'transfer-encoding',
  'upgrade',
  'host',
  'content-length',
  'accept-encoding',
  'expect',
]);

/** Response headers stripped before relaying to the caller. */
export const RESPONSE_HOP_BY_HOP = new Set(['connection', 'keep-alive', 'proxy-connection', 'transfer-encoding', 'upgrade', 'trailer', 'te']);

const httpAgent = new http.Agent({ keepAlive: true, maxSockets: 512 });
const httpsAgent = new https.Agent({ keepAlive: true, maxSockets: 512 });

export interface UpstreamState {
  failures: number;
  downUntil: number;
  lastError?: string;
  lastErrorAt?: string;
}

/**
 * Passive health tracking: after `threshold` consecutive failures an upstream
 * is considered down for `cooldownMs` and is tried last.
 */
export class UpstreamHealth {
  private readonly states = new Map<string, UpstreamState>();
  readonly threshold: number;
  readonly cooldownMs: number;

  constructor(threshold = 2, cooldownMs = 15_000) {
    this.threshold = threshold;
    this.cooldownMs = cooldownMs;
  }

  isDown(url: string, now = Date.now()): boolean {
    const s = this.states.get(url);
    return !!s && s.downUntil > now;
  }

  success(url: string): void {
    const s = this.states.get(url);
    if (s) {
      s.failures = 0;
      s.downUntil = 0;
    }
  }

  failure(url: string, reason: string): void {
    let s = this.states.get(url);
    if (!s) this.states.set(url, (s = { failures: 0, downUntil: 0 }));
    s.failures++;
    s.lastError = reason;
    s.lastErrorAt = new Date().toISOString();
    if (s.failures >= this.threshold) s.downUntil = Date.now() + this.cooldownMs;
  }

  get(url: string): { healthy: boolean } & Partial<UpstreamState> {
    const s = this.states.get(url);
    return { healthy: !this.isDown(url), ...s };
  }
}

/** Orders a route's upstreams for this attempt according to its strategy; healthy ones first. */
export function upstreamOrder(route: CompiledRoute, health: UpstreamHealth, random = Math.random): UpstreamDef[] {
  const ups = route.upstreams;
  let ordered: UpstreamDef[];
  if (route.strategy === 'failover' || ups.length === 1) {
    ordered = [...ups];
  } else {
    const totalWeight = ups.reduce((n, u) => n + u.weight, 0);
    let ticket = route.strategy === 'random' ? Math.floor(random() * totalWeight) : route.cursor++ % totalWeight;
    let first = 0;
    for (let i = 0; i < ups.length; i++) {
      if (ticket < ups[i]!.weight) {
        first = i;
        break;
      }
      ticket -= ups[i]!.weight;
    }
    ordered = [...ups.slice(first), ...ups.slice(0, first)];
  }
  const now = Date.now();
  return [...ordered.filter((u) => !health.isDown(u.url, now)), ...ordered.filter((u) => health.isDown(u.url, now))];
}

export function buildTargetUrl(base: string, path: string, query: string): string {
  const joined = path ? `${base.replace(/\/+$/, '')}${path}` : base;
  return `${joined}${query}`;
}

export function shouldRetry(route: Pick<RouteDef, 'retryOn'>, status: number | 'error'): boolean {
  if (status === 'error') return route.retryOn.includes('error');
  return route.retryOn.some((t) => (t === '5xx' ? status >= 500 : t === '4xx' ? status >= 400 && status < 500 : Number(t) === status));
}

export interface ForwardOptions {
  cookieName: string;
  /** Protocol the caller used to reach Moxy. */
  protocol: 'http' | 'https';
  signal?: AbortSignal;
}

export function buildUpstreamHeaders(ctx: Context, upstream: UpstreamDef, target: URL, opts: ForwardOptions): OutgoingHttpHeaders {
  const out: OutgoingHttpHeaders = {};
  const connectionTokens = new Set((ctx.req.headers.connection ?? '').split(',').map((t) => t.trim().toLowerCase()));
  for (const [k, v] of Object.entries(ctx.headers)) {
    if (HOP_BY_HOP.has(k) || connectionTokens.has(k)) continue;
    out[k] = v;
  }
  const cookie = removeCookie(ctx.headers.cookie, opts.cookieName);
  if (cookie) out.cookie = cookie;
  else delete out.cookie;

  out.host = ctx.route.preserveHost && ctx.req.headers.host ? ctx.req.headers.host : target.host;
  const priorXff = ctx.headers['x-forwarded-for'];
  out['x-forwarded-for'] = priorXff ? `${priorXff}, ${ctx.clientIp}` : ctx.clientIp;
  out['x-forwarded-proto'] = opts.protocol;
  if (ctx.req.headers.host) out['x-forwarded-host'] = ctx.req.headers.host;
  out['x-request-id'] = ctx.id;
  // Identity encoding lets Moxy observe token usage in responses.
  out['accept-encoding'] = 'identity';
  const body = ctx.finalBody();
  if (body.length > 0 || !['GET', 'HEAD', 'DELETE', 'OPTIONS'].includes(ctx.method)) out['content-length'] = body.length;
  for (const [name, value] of upstream.headers) out[name] = value;
  return out;
}

export type ForwardResult =
  | {
      ok: true;
      status: number;
      headers: IncomingHttpHeaders;
      stream: IncomingMessage;
      request: ClientRequest;
      upstream: UpstreamDef;
      attempts: number;
    }
  | { ok: false; status: number; code: string; message: string; attempts: number };

type Attempt =
  | { ok: true; response: IncomingMessage; request: ClientRequest }
  | { ok: false; timeout: boolean; aborted: boolean; message: string };

function attemptOnce(ctx: Context, upstream: UpstreamDef, opts: ForwardOptions): Promise<Attempt> {
  return new Promise((resolve) => {
    const target = new URL(buildTargetUrl(upstream.url, ctx.upstreamPath, ctx.query));
    ctx.upstreamUrl = `${target.origin}${target.pathname}`;
    const isHttps = target.protocol === 'https:';
    const headers = buildUpstreamHeaders(ctx, upstream, target, opts);
    let settled = false;
    let timedOut = false;
    const req = (isHttps ? https : http).request(target, {
      method: ctx.method,
      headers,
      agent: isHttps ? httpsAgent : httpAgent,
      signal: opts.signal,
    });
    req.setTimeout(ctx.timeoutMs ?? upstream.timeoutMs ?? DEFAULT_TIMEOUT_MS, () => {
      timedOut = true;
      req.destroy(new Error('Upstream timed out'));
    });
    req.on('response', (response) => {
      settled = true;
      resolve({ ok: true, response, request: req });
    });
    req.on('error', (err) => {
      if (settled) return;
      settled = true;
      resolve({ ok: false, timeout: timedOut, aborted: !!opts.signal?.aborted, message: err.message });
    });
    const body = ctx.finalBody();
    req.end(body.length ? body : undefined);
  });
}

/** Sends the request upstream, retrying across upstreams per the route's strategy and retry-on rules. */
export async function forward(ctx: Context, health: UpstreamHealth, opts: ForwardOptions): Promise<ForwardResult> {
  const route = ctx.route;
  const order = upstreamOrder(route, health);
  const maxAttempts = route.retries + 1;
  let failure: ForwardResult = { ok: false, status: 502, code: 'upstream_unavailable', message: 'No upstream available', attempts: 0 };
  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    const upstream = order[attempt % order.length]!;
    const last = attempt === maxAttempts - 1;
    const r = await attemptOnce(ctx, upstream, opts);
    if (r.ok) {
      const status = r.response.statusCode ?? 502;
      if (status >= 500) health.failure(upstream.url, `HTTP ${status}`);
      else health.success(upstream.url);
      if (!last && shouldRetry(route, status)) {
        r.response.resume();
        r.request.destroy();
        continue;
      }
      return { ok: true, status, headers: r.response.headers, stream: r.response, request: r.request, upstream, attempts: attempt + 1 };
    }
    if (r.aborted) return { ok: false, status: 499, code: 'client_closed', message: 'Client closed the connection', attempts: attempt + 1 };
    health.failure(upstream.url, r.message);
    failure = r.timeout
      ? { ok: false, status: 504, code: 'upstream_timeout', message: 'Upstream did not respond in time', attempts: attempt + 1 }
      : { ok: false, status: 502, code: 'upstream_unavailable', message: `Upstream request failed: ${r.message}`, attempts: attempt + 1 };
    if (!shouldRetry(route, 'error')) break;
  }
  return failure;
}
