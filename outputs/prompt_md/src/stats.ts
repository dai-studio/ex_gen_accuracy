import type { Usage } from './usage.ts';

export interface Counter {
  requests: number;
  errors: number;
  status: Record<'1xx' | '2xx' | '3xx' | '4xx' | '5xx', number>;
  totalLatencyMs: number;
  maxLatencyMs: number;
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  lastRequestAt?: string;
}

export interface RequestRecord {
  routeId?: string;
  principalId?: string;
  model?: string;
  status: number;
  latencyMs: number;
  usage?: Usage | null;
}

function emptyCounter(): Counter {
  return {
    requests: 0,
    errors: 0,
    status: { '1xx': 0, '2xx': 0, '3xx': 0, '4xx': 0, '5xx': 0 },
    totalLatencyMs: 0,
    maxLatencyMs: 0,
    promptTokens: 0,
    completionTokens: 0,
    totalTokens: 0,
  };
}

function bump(map: Map<string, Counter>, key: string, r: RequestRecord): void {
  let c = map.get(key);
  if (!c) map.set(key, (c = emptyCounter()));
  add(c, r);
}

function add(c: Counter, r: RequestRecord): void {
  c.requests++;
  if (r.status >= 500) c.errors++;
  const cls = `${Math.min(5, Math.max(1, Math.floor(r.status / 100)))}xx` as keyof Counter['status'];
  c.status[cls]++;
  c.totalLatencyMs += r.latencyMs;
  c.maxLatencyMs = Math.max(c.maxLatencyMs, r.latencyMs);
  if (r.usage) {
    c.promptTokens += r.usage.promptTokens;
    c.completionTokens += r.usage.completionTokens;
    c.totalTokens += r.usage.totalTokens;
  }
  c.lastRequestAt = new Date().toISOString();
}

/** In-memory request and token counters per route, principal and model. Survives config reloads. */
export class Stats {
  private routes = new Map<string, Counter>();
  private principals = new Map<string, Counter>();
  private models = new Map<string, Counter>();
  private total = emptyCounter();
  private since = new Date().toISOString();

  record(r: RequestRecord): void {
    if (r.routeId) bump(this.routes, r.routeId, r);
    if (r.principalId) bump(this.principals, r.principalId, r);
    if (r.model) bump(this.models, r.model, r);
    add(this.total, r);
  }

  reset(): void {
    this.routes.clear();
    this.principals.clear();
    this.models.clear();
    this.total = emptyCounter();
    this.since = new Date().toISOString();
  }

  snapshot(): {
    since: string;
    total: Counter & { avgLatencyMs: number };
    routes: Record<string, Counter & { avgLatencyMs: number }>;
    principals: Record<string, Counter & { avgLatencyMs: number }>;
    models: Record<string, Counter & { avgLatencyMs: number }>;
  } {
    const withAvg = (c: Counter) => ({ ...c, status: { ...c.status }, avgLatencyMs: c.requests ? Math.round(c.totalLatencyMs / c.requests) : 0 });
    const obj = (m: Map<string, Counter>) => Object.fromEntries([...m].map(([k, c]) => [k, withAvg(c)]));
    return { since: this.since, total: withAvg(this.total), routes: obj(this.routes), principals: obj(this.principals), models: obj(this.models) };
  }
}
