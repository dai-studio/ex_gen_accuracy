import type { IncomingMessage } from 'node:http';
import type { AuthResult, AuthService, Principal } from './auth.ts';
import type { CompiledRoute } from './middleware/types.ts';
import type { Usage } from './usage.ts';

export interface ContextInit {
  id: string;
  req: IncomingMessage;
  route: CompiledRoute;
  /** Request path as received. */
  path: string;
  /** Raw query string including the leading "?", or "". */
  query: string;
  /** Path appended to the upstream URL. */
  upstreamPath: string;
  body: Buffer;
  clientIp: string;
  auth: AuthService;
  method?: string;
  headers?: Record<string, string>;
}

/** Per-request state shared by the middleware chain and the proxy. */
export class Context {
  readonly id: string;
  readonly req: IncomingMessage;
  readonly route: CompiledRoute;
  readonly method: string;
  readonly path: string;
  readonly query: string;
  readonly clientIp: string;
  readonly startedAt = Date.now();
  /** Arbitrary per-request scratch space for middleware. */
  readonly state = new Map<string, unknown>();
  /** Outgoing upstream request headers (lower-case names). */
  headers: Record<string, string>;
  upstreamPath: string;
  body: Buffer;
  timeoutMs?: number;
  /** Model name as requested by the caller (before aliasing). */
  model?: string;
  usage: Usage | null = null;
  upstreamUrl?: string;

  private readonly auth: AuthService;
  private authResult?: AuthResult;
  private json: unknown;
  private jsonState: 'unparsed' | 'parsed' | 'invalid' = 'unparsed';
  private jsonDirty = false;
  private readonly doneHooks: Array<(ctx: Context) => void> = [];

  constructor(init: ContextInit) {
    this.id = init.id;
    this.req = init.req;
    this.route = init.route;
    this.method = (init.method ?? init.req.method ?? 'GET').toUpperCase();
    this.path = init.path;
    this.query = init.query;
    this.upstreamPath = init.upstreamPath;
    this.body = init.body;
    this.clientIp = init.clientIp;
    this.auth = init.auth;
    this.headers = init.headers ?? flattenHeaders(init.req.headers);
  }

  /** Resolves (once) the caller's credentials. */
  resolveAuth(): AuthResult {
    this.authResult ??= this.auth.resolve(this.req);
    return this.authResult;
  }

  get principal(): Principal | null {
    return this.resolveAuth().principal;
  }

  get isJson(): boolean {
    return /\bjson\b/i.test(this.headers['content-type'] ?? '');
  }

  /** The parsed JSON request body, or undefined if the body is absent or not JSON. */
  getJson(): any {
    if (this.jsonState === 'unparsed') {
      this.jsonState = 'invalid';
      if (this.body.length && (this.isJson || this.body[0] === 0x7b)) {
        try {
          this.json = JSON.parse(this.body.toString('utf8'));
          this.jsonState = 'parsed';
        } catch {
          /* not JSON */
        }
      }
    }
    return this.jsonState === 'parsed' ? this.json : undefined;
  }

  /** Replaces the JSON body; it is re-serialised before forwarding. */
  setJson(value: unknown): void {
    this.json = value;
    this.jsonState = 'parsed';
    this.jsonDirty = true;
  }

  /** Marks the object returned by getJson() as modified in place. */
  touchJson(): void {
    if (this.jsonState === 'parsed') this.jsonDirty = true;
  }

  /** Body bytes to forward upstream. */
  finalBody(): Buffer {
    if (this.jsonDirty) {
      this.body = Buffer.from(JSON.stringify(this.json));
      this.jsonDirty = false;
    }
    return this.body;
  }

  /** Registers a callback that runs once the response has been fully sent (usage is known by then). */
  onDone(fn: (ctx: Context) => void): void {
    this.doneHooks.push(fn);
  }

  runDone(): void {
    for (const fn of this.doneHooks.splice(0)) {
      try {
        fn(this);
      } catch {
        /* hooks must not break response handling */
      }
    }
  }
}

export function flattenHeaders(headers: IncomingMessage['headers']): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(headers)) {
    if (v === undefined) continue;
    out[k.toLowerCase()] = Array.isArray(v) ? v.join(k === 'cookie' ? '; ' : ', ') : v;
  }
  return out;
}
