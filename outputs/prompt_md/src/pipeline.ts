import type { IncomingMessage, OutgoingHttpHeaders, ServerResponse } from 'node:http';
import { pipeline } from 'node:stream/promises';
import type { Context } from './context.ts';
import type { Logger } from './logger.ts';
import type { Middleware, ProxyResponse, Reply } from './middleware/types.ts';
import { forward, RESPONSE_HOP_BY_HOP, type UpstreamHealth } from './proxy.ts';
import { scanBody, UsageTap } from './usage.ts';
import { errorBody, HttpError } from './util.ts';

export interface PipelineDeps {
  health: UpstreamHealth;
  logger: Logger;
  cookieName: string;
  protocol: 'http' | 'https';
}

const MAX_BUFFERED_RESPONSE = 32 * 1024 * 1024;

export function replyToResponse(reply: Reply): ProxyResponse {
  const headers: OutgoingHttpHeaders = {};
  for (const [k, v] of Object.entries(reply.headers ?? {})) headers[k.toLowerCase()] = v;
  let body: Buffer;
  if (reply.body === undefined || reply.body === null) body = Buffer.alloc(0);
  else if (Buffer.isBuffer(reply.body)) body = reply.body;
  else if (typeof reply.body === 'string') {
    body = Buffer.from(reply.body);
    headers['content-type'] ??= 'text/plain; charset=utf-8';
  } else {
    body = Buffer.from(JSON.stringify(reply.body));
    headers['content-type'] ??= 'application/json; charset=utf-8';
  }
  return { status: reply.status, headers, body, source: 'moxy' };
}

function errorResponse(status: number, code: string, message: string, headers: OutgoingHttpHeaders = {}): ProxyResponse {
  return replyToResponse({ status, headers, body: errorBody(message, code) });
}

function readAll(stream: IncomingMessage, limit: number): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    stream.on('data', (c: Buffer) => {
      size += c.length;
      if (size > limit) {
        stream.destroy();
        reject(new Error('Upstream response too large to buffer'));
      } else chunks.push(c);
    });
    stream.on('end', () => resolve(Buffer.concat(chunks)));
    stream.on('error', reject);
    stream.on('aborted', () => reject(new Error('Upstream response aborted')));
  });
}

/** Rewrites an aliased model name to the name the upstream expects. */
function applyModelAlias(ctx: Context): void {
  const body = ctx.getJson();
  if (!body || typeof body !== 'object' || typeof body.model !== 'string') return;
  const alias = ctx.route.models?.models.find((m) => m.id === body.model);
  if (alias?.upstreamModel && alias.upstreamModel !== body.model) {
    body.model = alias.upstreamModel;
    ctx.touchJson();
  }
}

/**
 * Runs a route: request middleware in order, the upstream call (unless a
 * middleware replied), response middleware in reverse order, then relays the
 * response (streamed, or buffered when a middleware asked for the body).
 */
export async function runPipeline(ctx: Context, res: ServerResponse, deps: PipelineDeps): Promise<void> {
  const chain: Middleware[] = ctx.route.chain;
  const json = ctx.getJson();
  if (json && typeof json.model === 'string') ctx.model = json.model;

  let response: ProxyResponse | null = null;
  let ran = 0;
  try {
    for (; ran < chain.length; ran++) {
      const mw = chain[ran]!;
      if (!mw.onRequest) continue;
      const reply = await mw.onRequest(ctx);
      if (reply) {
        response = replyToResponse(reply);
        break;
      }
    }
  } catch (err) {
    response = middlewareError(err, deps.logger, ctx);
  }

  let stream: IncomingMessage | undefined;
  if (!response) {
    applyModelAlias(ctx);
    const abort = new AbortController();
    const onClose = () => {
      if (!res.writableFinished) abort.abort();
    };
    res.once('close', onClose);
    const result = await forward(ctx, deps.health, { cookieName: deps.cookieName, protocol: deps.protocol, signal: abort.signal });
    res.off('close', onClose);
    if (!result.ok) {
      if (result.status === 499) {
        res.statusCode = 499;
        ctx.runDone();
        return;
      }
      response = errorResponse(result.status, result.code, result.message);
    } else {
      const headers: OutgoingHttpHeaders = {};
      for (const [k, v] of Object.entries(result.headers)) if (!RESPONSE_HOP_BY_HOP.has(k) && v !== undefined) headers[k] = v;
      // Keep Moxy's request id (it matches the access log); expose the provider's separately.
      if (headers['x-request-id'] !== undefined) {
        headers['x-upstream-request-id'] = headers['x-request-id'];
        delete headers['x-request-id'];
      }
      response = { status: result.status, headers, source: 'upstream' };
      const contentType = String(result.headers['content-type'] ?? '');
      const wantsBody = chain.some((m) => m.bufferResponse) && !contentType.includes('text/event-stream');
      if (wantsBody) {
        try {
          response.body = await readAll(result.stream, MAX_BUFFERED_RESPONSE);
        } catch (err) {
          response = errorResponse(502, 'upstream_unavailable', (err as Error).message);
        }
        if (response.body && response.source === 'upstream') {
          const scan = scanBody(contentType, response.body);
          ctx.usage = scan.usage;
          ctx.model ??= scan.model;
        }
      } else {
        stream = result.stream;
      }
    }
  }

  for (let i = ran - 1; i >= 0; i--) {
    const mw = chain[i]!;
    if (!mw.onResponse) continue;
    try {
      await mw.onResponse(ctx, response);
    } catch (err) {
      deps.logger.error('response middleware failed', { id: ctx.id, middleware: mw.type, error: (err as Error).message });
    }
  }

  await send(ctx, res, response, stream);
}

function middlewareError(err: unknown, logger: Logger, ctx: Context): ProxyResponse {
  if (err instanceof HttpError) return errorResponse(err.status, err.code, err.message, err.headers);
  logger.error('request middleware failed', { id: ctx.id, error: (err as Error).stack ?? String(err) });
  return errorResponse(500, 'internal_error', 'Internal gateway error');
}

async function send(ctx: Context, res: ServerResponse, response: ProxyResponse, stream?: IncomingMessage): Promise<void> {
  const headOnly = ctx.method === 'HEAD';
  try {
    if (!stream) {
      const body = response.body ?? Buffer.alloc(0);
      response.headers['content-length'] = body.length;
      res.writeHead(response.status, response.headers);
      res.end(headOnly ? undefined : body);
      return;
    }
    res.writeHead(response.status, response.headers);
    const tap = new UsageTap(String(response.headers['content-type'] ?? ''));
    try {
      await pipeline(stream, tap, res);
    } catch {
      // Client went away or the upstream broke mid-stream; the socket is already torn down.
    }
    ctx.usage = tap.scanner.usage;
    ctx.model ??= tap.scanner.model;
  } finally {
    ctx.runDone();
  }
}
