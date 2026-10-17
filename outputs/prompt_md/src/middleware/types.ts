import type { OutgoingHttpHeaders } from 'node:http';
import type { AuthService, Principal } from '../auth.ts';
import type { MiddlewareDef, RouteDef } from '../config.ts';
import type { Context } from '../context.ts';
import type { Logger } from '../logger.ts';
import { errorBody } from '../util.ts';

/** A response produced by Moxy itself, short-circuiting the upstream call. */
export interface Reply {
  status: number;
  headers?: OutgoingHttpHeaders;
  /** Objects are serialised as JSON; strings and Buffers are sent as-is. */
  body?: unknown;
}

/** The response travelling back through the middleware chain. Header names are lower-case. */
export interface ProxyResponse {
  status: number;
  headers: OutgoingHttpHeaders;
  /** Present only when the response was buffered (or produced by Moxy). */
  body?: Buffer;
  source: 'upstream' | 'moxy';
}

export interface Middleware {
  readonly type: string;
  /** Ask the gateway to buffer non-streaming upstream bodies so `onResponse` sees `res.body`. */
  readonly bufferResponse?: boolean;
  /** Runs in chain order before the upstream call. Returning a Reply stops the chain. */
  onRequest?(ctx: Context): void | Reply | Promise<void | Reply>;
  /** Runs in reverse chain order once a response exists (upstream or short-circuit). */
  onResponse?(ctx: Context, res: ProxyResponse): void | Promise<void>;
  /** Access check used by discovery and model listing, without running the request. */
  allows?(principal: Principal | null): boolean;
  /** Public description of the middleware's effect, shown by discovery. */
  readonly info?: Record<string, unknown>;
}

export interface MiddlewareEnv {
  auth: AuthService;
  logger: Logger;
  /** Route id, or "global" for top-level middleware. */
  scope: string;
}

export type MiddlewareFactory = (def: MiddlewareDef, env: MiddlewareEnv) => Middleware;

export interface CompiledRoute extends RouteDef {
  chain: Middleware[];
  /** Round-robin cursor. */
  cursor: number;
}

export function errorReply(status: number, code: string, message: string, headers: OutgoingHttpHeaders = {}): Reply {
  return { status, headers, body: errorBody(message, code) };
}
