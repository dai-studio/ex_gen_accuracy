import { ConfigError, checkAttrs, type MiddlewareDef } from '../config.ts';
import type { Context } from '../context.ts';

/** Validates attribute names and returns the attribute map. */
export function attrs(def: MiddlewareDef, allowed: string[]): Record<string, string | undefined> {
  checkAttrs(def.element, allowed);
  return def.attrs;
}

export function requireAttr(def: MiddlewareDef, name: string): string {
  const v = def.attrs[name];
  if (v === undefined || v.trim() === '') throw new ConfigError(`<${def.type}> requires a "${name}" attribute`, def.element);
  return v;
}

/** Wraps value parsing so failures point at the middleware element. */
export function parsed<T>(def: MiddlewareDef, fn: () => T): T {
  try {
    return fn();
  } catch (err) {
    if (err instanceof ConfigError) throw err;
    throw new ConfigError(`<${def.type}>: ${(err as Error).message}`, def.element);
  }
}

export function oneOf<T extends string>(def: MiddlewareDef, name: string, value: string | undefined, options: readonly T[], fallback: T): T {
  if (value === undefined || value === '') return fallback;
  if (!options.includes(value as T)) {
    throw new ConfigError(`<${def.type}> ${name} must be one of ${options.join(', ')} (got "${value}")`, def.element);
  }
  return value as T;
}

export type KeySpec = 'principal' | 'ip' | 'global' | 'route' | `header:${string}`;

export function parseKeySpec(def: MiddlewareDef, value: string | undefined): KeySpec {
  const v = value ?? 'principal';
  if (['principal', 'ip', 'global', 'route'].includes(v) || /^header:[A-Za-z0-9-]+$/.test(v)) return v as KeySpec;
  throw new ConfigError(`<${def.type}> key must be principal, ip, route, global or header:<name> (got "${v}")`, def.element);
}

/** Bucket key for rate limiting / quotas. Anonymous callers fall back to their IP. */
export function bucketKey(ctx: Context, spec: KeySpec): string {
  switch (spec) {
    case 'global':
      return 'global';
    case 'route':
      return `route:${ctx.route.id}`;
    case 'ip':
      return `ip:${ctx.clientIp}`;
    case 'principal': {
      const p = ctx.principal;
      return p ? p.id : `ip:${ctx.clientIp}`;
    }
    default: {
      const header = spec.slice(7).toLowerCase();
      return `header:${ctx.req.headers[header] ?? ''}`;
    }
  }
}

/**
 * Expands placeholders in header values: {principal.id}, {principal.name},
 * {principal.roles}, {request.id}, {client.ip}, {route.id}, {model}.
 * Unknown placeholders are left untouched.
 */
export function expandTemplate(template: string, ctx: Context): string {
  if (!template.includes('{')) return template;
  return template.replace(/\{([a-z]+(?:\.[a-z]+)?)\}/g, (match, key: string) => {
    switch (key) {
      case 'principal.id':
        return ctx.principal?.id ?? '';
      case 'principal.name':
        return ctx.principal?.name ?? '';
      case 'principal.roles':
        return ctx.principal?.roles.join(',') ?? '';
      case 'request.id':
        return ctx.id;
      case 'client.ip':
        return ctx.clientIp;
      case 'route.id':
        return ctx.route.id;
      case 'model':
        return ctx.model ?? '';
      default:
        return match;
    }
  });
}

/** Approximate sliding-window counter (weighted current + previous fixed windows). */
export class SlidingWindowLimiter {
  readonly limit: number;
  readonly windowMs: number;
  private readonly buckets = new Map<string, { start: number; count: number; prev: number }>();

  constructor(limit: number, windowMs: number) {
    this.limit = limit;
    this.windowMs = windowMs;
  }

  /** Whether one more hit would be allowed, without counting it. */
  wouldAllow(key: string, now = Date.now()): boolean {
    const b = this.buckets.get(key);
    if (!b) return true;
    const start = Math.floor(now / this.windowMs) * this.windowMs;
    const count = b.start === start ? b.count : 0;
    const prev = b.start === start ? b.prev : b.start === start - this.windowMs ? b.count : 0;
    return prev * ((this.windowMs - (now - start)) / this.windowMs) + count + 1 <= this.limit;
  }

  hit(key: string, now = Date.now()): { allowed: boolean; remaining: number; resetMs: number } {
    const start = Math.floor(now / this.windowMs) * this.windowMs;
    let b = this.buckets.get(key);
    if (!b) {
      if (this.buckets.size >= 10_000) this.prune(start);
      b = { start, count: 0, prev: 0 };
      this.buckets.set(key, b);
    } else if (b.start !== start) {
      b.prev = b.start === start - this.windowMs ? b.count : 0;
      b.count = 0;
      b.start = start;
    }
    const elapsed = now - start;
    const estimate = b.prev * ((this.windowMs - elapsed) / this.windowMs) + b.count;
    const resetMs = this.windowMs - elapsed;
    if (estimate + 1 > this.limit) return { allowed: false, remaining: 0, resetMs };
    b.count++;
    return { allowed: true, remaining: Math.max(0, Math.floor(this.limit - estimate - 1)), resetMs };
  }

  private prune(currentStart: number): void {
    for (const [k, b] of this.buckets) if (b.start < currentStart - this.windowMs) this.buckets.delete(k);
  }
}
