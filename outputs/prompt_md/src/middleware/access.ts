import { BlockList, isIPv4, isIPv6 } from 'node:net';
import { hasRole } from '../auth.ts';
import { ConfigError } from '../config.ts';
import { globToRegExp, matchesAny, parseBool, parseDuration, parseIntStrict, parseSize, splitList } from '../util.ts';
import { attrs, bucketKey, parseKeySpec, parsed, requireAttr, SlidingWindowLimiter } from './helpers.ts';
import { errorReply, type MiddlewareFactory } from './types.ts';

/**
 * <auth roles="developer,agent" optional="false" strip-credentials="true"/>
 * Requires a Moxy principal (token, API key or session cookie) with one of the roles.
 */
export const auth: MiddlewareFactory = (def, env) => {
  const a = attrs(def, ['roles', 'optional', 'strip-credentials']);
  const roles = splitList(a.roles);
  const optional = parsed(def, () => parseBool(a.optional, false));
  const strip = parsed(def, () => parseBool(a['strip-credentials'], true));
  if (!env.auth.enabled && !optional) {
    env.logger.warn('auth middleware configured but no users or api-keys exist; requests will be rejected', { scope: env.scope });
  }
  return {
    type: 'auth',
    info: { roles, optional },
    allows: (principal) => optional || (!!principal && hasRole(principal, roles)),
    onRequest(ctx) {
      const { principal, error } = ctx.resolveAuth();
      if (principal && strip) {
        delete ctx.headers.authorization;
        delete ctx.headers['x-api-key'];
      }
      if (!principal) {
        if (optional) return;
        return errorReply(401, error ? 'invalid_credentials' : 'unauthorized', error ?? 'Authentication required', {
          'www-authenticate': 'Bearer realm="moxy"',
        });
      }
      if (!hasRole(principal, roles)) {
        return errorReply(403, 'forbidden', `Requires one of the roles: ${roles.join(', ')}`);
      }
    },
  };
};

function normaliseIp(ip: string): string {
  const mapped = /^::ffff:(\d+\.\d+\.\d+\.\d+)$/i.exec(ip);
  return mapped ? mapped[1]! : ip;
}

function buildBlockList(entries: string[], fail: (msg: string) => never): BlockList {
  const list = new BlockList();
  for (const entry of entries) {
    const [addr = '', prefix] = entry.split('/');
    const type = isIPv4(addr) ? 'ipv4' : isIPv6(addr) ? 'ipv6' : fail(`Invalid IP address or CIDR "${entry}"`);
    if (prefix === undefined) {
      list.addAddress(addr, type);
    } else {
      const bits = Number(prefix);
      if (!/^\d+$/.test(prefix) || bits > (type === 'ipv4' ? 32 : 128)) fail(`Invalid CIDR prefix in "${entry}"`);
      list.addSubnet(addr, bits, type);
    }
  }
  return list;
}

/** <ip-filter allow="10.0.0.0/8, ::1" deny="10.0.0.13"/> */
export const ipFilter: MiddlewareFactory = (def) => {
  const a = attrs(def, ['allow', 'deny']);
  const fail = (msg: string): never => {
    throw new ConfigError(`<ip-filter>: ${msg}`, def.element);
  };
  const allowEntries = splitList(a.allow);
  const denyEntries = splitList(a.deny);
  if (!allowEntries.length && !denyEntries.length) fail('needs an allow or deny list');
  const allow = buildBlockList(allowEntries, fail);
  const deny = buildBlockList(denyEntries, fail);
  return {
    type: 'ip-filter',
    onRequest(ctx) {
      const ip = normaliseIp(ctx.clientIp);
      const type = isIPv4(ip) ? 'ipv4' : 'ipv6';
      let blocked: boolean;
      try {
        blocked = deny.check(ip, type) || (allowEntries.length > 0 && !allow.check(ip, type));
      } catch {
        blocked = true;
      }
      if (blocked) return errorReply(403, 'ip_forbidden', 'Your address is not allowed to use this route');
    },
  };
};

/** <rate-limit requests="60" window="1m" key="principal|ip|route|global|header:X-Name"/> */
export const rateLimit: MiddlewareFactory = (def) => {
  const a = attrs(def, ['requests', 'window', 'key']);
  const limit = parsed(def, () => parseIntStrict(requireAttr(def, 'requests'), 'requests', 1));
  const windowMs = parsed(def, () => parseDuration(a.window ?? '1m'));
  if (windowMs < 1) throw new ConfigError('<rate-limit> window must be positive', def.element);
  const key = parseKeySpec(def, a.key);
  const limiter = new SlidingWindowLimiter(limit, windowMs);
  return {
    type: 'rate-limit',
    onRequest(ctx) {
      const r = limiter.hit(bucketKey(ctx, key));
      const headers = {
        'x-ratelimit-limit': String(limit),
        'x-ratelimit-remaining': String(r.remaining),
        'x-ratelimit-reset': String(Math.ceil(r.resetMs / 1000)),
      };
      if (!r.allowed) {
        return errorReply(429, 'rate_limited', `Rate limit of ${limit} requests per ${a.window ?? '1m'} exceeded`, {
          ...headers,
          'retry-after': String(Math.max(1, Math.ceil(r.resetMs / 1000))),
        });
      }
      ctx.state.set('rate-limit.headers', headers);
    },
    onResponse(ctx, res) {
      const headers = ctx.state.get('rate-limit.headers') as Record<string, string> | undefined;
      if (headers) Object.assign(res.headers, headers);
    },
  };
};

/** <quota tokens="200000" window="1d" key="principal"/> — token budget fed by observed usage. */
export const quota: MiddlewareFactory = (def) => {
  const a = attrs(def, ['tokens', 'window', 'key']);
  const limit = parsed(def, () => parseIntStrict(requireAttr(def, 'tokens'), 'tokens', 1));
  const windowMs = parsed(def, () => parseDuration(a.window ?? '1d'));
  const key = parseKeySpec(def, a.key);
  const buckets = new Map<string, { start: number; used: number }>();
  const bucket = (k: string) => {
    const now = Date.now();
    const start = Math.floor(now / windowMs) * windowMs;
    let b = buckets.get(k);
    if (!b || b.start !== start) {
      if (!b && buckets.size >= 10_000) for (const [bk, v] of buckets) if (v.start !== start) buckets.delete(bk);
      b = { start, used: 0 };
      buckets.set(k, b);
    }
    return b;
  };
  return {
    type: 'quota',
    onRequest(ctx) {
      const k = bucketKey(ctx, key);
      const b = bucket(k);
      const resetSec = Math.max(1, Math.ceil((b.start + windowMs - Date.now()) / 1000));
      if (b.used >= limit) {
        return errorReply(429, 'quota_exceeded', `Token quota of ${limit} per ${a.window ?? '1d'} exhausted`, {
          'retry-after': String(resetSec),
          'x-moxy-quota-limit': String(limit),
          'x-moxy-quota-remaining': '0',
        });
      }
      ctx.state.set('quota.remaining', limit - b.used);
      ctx.onDone((done) => {
        if (done.usage) bucket(k).used += done.usage.totalTokens;
      });
    },
    onResponse(ctx, res) {
      const remaining = ctx.state.get('quota.remaining');
      if (remaining !== undefined) {
        res.headers['x-moxy-quota-limit'] = String(limit);
        res.headers['x-moxy-quota-remaining'] = String(remaining);
      }
    },
  };
};

/** <body-limit max="1mb"/> */
export const bodyLimit: MiddlewareFactory = (def) => {
  attrs(def, ['max']);
  const max = parsed(def, () => parseSize(requireAttr(def, 'max')));
  return {
    type: 'body-limit',
    onRequest(ctx) {
      if (ctx.body.length > max) return errorReply(413, 'payload_too_large', `Request body exceeds ${max} bytes`);
    },
  };
};

/** <cors origins="https://app.example.com" credentials="true" .../> */
export const cors: MiddlewareFactory = (def) => {
  const a = attrs(def, ['origins', 'methods', 'headers', 'expose', 'credentials', 'max-age']);
  const origins = splitList(a.origins ?? '*');
  const wildcard = origins.includes('*');
  const patterns = origins.filter((o) => o !== '*').map(globToRegExp);
  const methods = splitList(a.methods ?? 'GET,POST,PUT,PATCH,DELETE,OPTIONS').map((m) => m.toUpperCase()).join(', ');
  const allowHeaders = a.headers ?? '*';
  const expose = splitList(a.expose).join(', ');
  const credentials = parsed(def, () => parseBool(a.credentials, false));
  const maxAge = String(parsed(def, () => Math.round(parseDuration(a['max-age'] ?? '10m') / 1000)));

  const allowedOrigin = (origin: string | undefined): string | null => {
    if (!origin) return null;
    if (wildcard) return credentials ? origin : '*';
    return matchesAny(origin, patterns) ? origin : null;
  };
  const varyOrigin = (existing: unknown): string => {
    const parts = splitList(typeof existing === 'string' ? existing : '');
    return parts.some((p) => p.toLowerCase() === 'origin') ? parts.join(', ') : [...parts, 'Origin'].join(', ');
  };

  return {
    type: 'cors',
    onRequest(ctx) {
      const origin = ctx.req.headers.origin;
      if (ctx.method !== 'OPTIONS' || !origin || !ctx.req.headers['access-control-request-method']) return;
      const allowed = allowedOrigin(origin);
      if (!allowed) return { status: 204, headers: { vary: 'Origin' } };
      const headers: Record<string, string> = {
        'access-control-allow-origin': allowed,
        'access-control-allow-methods': methods,
        'access-control-allow-headers':
          allowHeaders === '*' ? (ctx.req.headers['access-control-request-headers'] as string | undefined) ?? '*' : allowHeaders,
        'access-control-max-age': maxAge,
        vary: 'Origin',
      };
      if (credentials) headers['access-control-allow-credentials'] = 'true';
      return { status: 204, headers };
    },
    onResponse(ctx, res) {
      const allowed = allowedOrigin(ctx.req.headers.origin);
      res.headers.vary = varyOrigin(res.headers.vary);
      delete res.headers['access-control-allow-origin'];
      if (!allowed) return;
      res.headers['access-control-allow-origin'] = allowed;
      if (credentials) res.headers['access-control-allow-credentials'] = 'true';
      if (expose) res.headers['access-control-expose-headers'] = expose;
    },
  };
};
