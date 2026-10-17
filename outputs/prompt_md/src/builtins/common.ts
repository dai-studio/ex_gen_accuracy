import { readFileSync } from 'node:fs';
import type { IncomingMessage, ServerResponse } from 'node:http';
import type { AuthResult, Principal } from '../auth.ts';
import type { Context } from '../context.ts';
import type { Gateway } from '../gateway.ts';
import type { Runtime } from '../runtime.ts';
import { escapeHtml, HttpError, randomId, readBody } from '../util.ts';

export interface BuiltinContext {
  req: IncomingMessage;
  res: ServerResponse;
  url: URL;
  path: string;
  method: string;
  id: string;
  clientIp: string;
  /** True when the caller reached Moxy over HTTPS (directly or via a trusted proxy). */
  secure: boolean;
  gateway: Gateway;
  runtime: Runtime;
  auth(): AuthResult;
  principal(): Principal | null;
  /** Lets the gateway attribute stats/logs when a builtin dispatches into a route. */
  attach(ctx: Context): void;
}

export type BuiltinHandler = (bc: BuiltinContext) => Promise<void> | void;

const UI_DIR = new URL('../../ui/', import.meta.url);
const templates = new Map<string, string>();

function template(name: string): string {
  let t = templates.get(name);
  if (t === undefined) {
    t = readFileSync(new URL(name, UI_DIR), 'utf8');
    templates.set(name, t);
  }
  return t;
}

/** Renders ui/<name>.html. {{key}} is HTML-escaped; {{{key}}} is inserted raw. */
export function renderPage(name: string, vars: Record<string, string>): { html: string; nonce: string } {
  const nonce = randomId(16);
  const all: Record<string, string> = { ...vars, nonce, styles: template('styles.css') };
  const html = template(`${name}.html`)
    .replace(/\{\{\{(\w+)\}\}\}/g, (_m, k: string) => all[k] ?? '')
    .replace(/\{\{(\w+)\}\}/g, (_m, k: string) => escapeHtml(all[k] ?? ''));
  return { html, nonce };
}

export function sendPage(res: ServerResponse, name: string, vars: Record<string, string>, status = 200): void {
  const { html, nonce } = renderPage(name, vars);
  const body = Buffer.from(html);
  res.writeHead(status, {
    'content-type': 'text/html; charset=utf-8',
    'content-length': body.length,
    'cache-control': 'no-store',
    'content-security-policy': `default-src 'none'; script-src 'nonce-${nonce}'; style-src 'nonce-${nonce}'; connect-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'`,
    'x-content-type-options': 'nosniff',
    'x-frame-options': 'DENY',
    'referrer-policy': 'no-referrer',
  });
  res.end(body);
}

/** Reads a JSON or form-urlencoded body into a flat object. */
export async function readFields(req: IncomingMessage, limit = 64 * 1024): Promise<{ fields: Record<string, unknown>; form: boolean }> {
  const raw = (await readBody(req, limit)).toString('utf8');
  const ct = (req.headers['content-type'] ?? '').toLowerCase();
  if (ct.includes('application/x-www-form-urlencoded')) {
    return { fields: Object.fromEntries(new URLSearchParams(raw)), form: true };
  }
  if (!raw.trim()) return { fields: {}, form: false };
  try {
    const parsed = JSON.parse(raw);
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) return { fields: parsed, form: false };
  } catch {
    /* fall through */
  }
  throw new HttpError(400, 'invalid_body', 'Expected a JSON object or form-encoded body');
}

export function methodNotAllowed(allowed: string[]): HttpError {
  return new HttpError(405, 'method_not_allowed', `Use ${allowed.join(' or ')}`, { allow: allowed.join(', ') });
}
