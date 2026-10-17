import type { ServerResponse } from 'node:http';
import type { Principal } from '../auth.ts';
import { HttpError, parseDuration, redirect, safeNextPath, sendError, sendJson } from '../util.ts';
import { methodNotAllowed, readFields, sendPage, type BuiltinContext } from './common.ts';

function sessionCookie(name: string, value: string, maxAgeSec: number, secure: boolean): string {
  return [`${name}=${value}`, 'Path=/', 'HttpOnly', 'SameSite=Strict', `Max-Age=${maxAgeSec}`, ...(secure ? ['Secure'] : [])].join('; ');
}

export function describePrincipal(p: Principal): Record<string, unknown> {
  return {
    id: p.id,
    name: p.name,
    kind: p.kind,
    roles: p.roles,
    via: p.via,
    ...(p.tokenExpiresAt ? { token_expires_at: new Date(p.tokenExpiresAt).toISOString() } : {}),
  };
}

function loginFailed(res: ServerResponse, form: boolean, next: string, status: number, code: string, message: string): void {
  if (form) {
    redirect(res, `/auth/login?error=${encodeURIComponent(code)}&next=${encodeURIComponent(next)}`, 303);
    return;
  }
  sendError(res, status, code, message);
}

/**
 * /auth/login   GET: login page; POST: {username, password} -> token + session cookie
 * /auth/logout  POST: revoke the current token and clear the cookie
 * /auth/me      GET: the authenticated principal
 * /auth/token   POST: mint a (shorter-lived) token for the current principal, e.g. from an API key
 */
export async function authRoutes(bc: BuiltinContext): Promise<void> {
  const { res, method, gateway, runtime } = bc;
  const auth = runtime.auth;
  const title = runtime.config.builtins.title;

  switch (bc.path) {
    case '/auth':
    case '/auth/':
      redirect(res, '/auth/login');
      return;

    case '/auth/login': {
      if (method === 'GET' || method === 'HEAD') {
        const error = bc.url.searchParams.get('error');
        const messages: Record<string, string> = {
          invalid_credentials: 'Incorrect username or password.',
          too_many_attempts: 'Too many attempts. Try again in a few minutes.',
          missing_fields: 'Enter a username and password.',
        };
        sendPage(res, 'login', {
          title,
          next: safeNextPath(bc.url.searchParams.get('next'), '/chat'),
          error: error ? (messages[error] ?? 'Login failed.') : '',
          notice: auth.hasUsers ? '' : 'No users are configured. Add <user> entries to the <auth> section of the config.',
        });
        return;
      }
      if (method !== 'POST') throw methodNotAllowed(['GET', 'POST']);
      const { fields, form } = await readFields(bc.req);
      const next = safeNextPath(typeof fields.next === 'string' ? fields.next : null, '/chat');
      if (gateway.loginBlocked(bc.clientIp)) {
        loginFailed(res, form, next, 429, 'too_many_attempts', 'Too many login attempts; try again later');
        return;
      }
      const username = typeof fields.username === 'string' ? fields.username : '';
      const password = typeof fields.password === 'string' ? fields.password : '';
      if (!username || !password) {
        loginFailed(res, form, next, 400, 'missing_fields', 'username and password are required');
        return;
      }
      const principal = await auth.login(username, password);
      if (!principal) {
        gateway.recordLoginFailure(bc.clientIp);
        gateway.logger.warn('login failed', { user: username, ip: bc.clientIp });
        loginFailed(res, form, next, 401, 'invalid_credentials', 'Invalid username or password');
        return;
      }
      const issued = auth.issueToken(principal);
      const cookie = sessionCookie(auth.cookieName, issued.token, Math.floor((issued.expiresAt - Date.now()) / 1000), bc.secure);
      gateway.logger.info('login', { user: username, ip: bc.clientIp });
      if (form) {
        res.setHeader('set-cookie', cookie);
        redirect(res, next, 303);
        return;
      }
      sendJson(
        res,
        200,
        {
          token: issued.token,
          token_type: 'Bearer',
          expires_at: new Date(issued.expiresAt).toISOString(),
          user: { name: principal.name, roles: principal.roles },
        },
        { 'set-cookie': cookie },
      );
      return;
    }

    case '/auth/logout': {
      if (method !== 'POST') throw methodNotAllowed(['POST']);
      const principal = bc.principal();
      if (principal) auth.revokeToken(principal);
      const clear = sessionCookie(auth.cookieName, '', 0, bc.secure);
      if ((bc.req.headers['content-type'] ?? '').includes('application/x-www-form-urlencoded')) {
        res.setHeader('set-cookie', clear);
        redirect(res, '/auth/login', 303);
        return;
      }
      sendJson(res, 200, { ok: true }, { 'set-cookie': clear });
      return;
    }

    case '/auth/me': {
      if (method !== 'GET' && method !== 'HEAD') throw methodNotAllowed(['GET']);
      const { principal, error } = bc.auth();
      if (!principal) throw new HttpError(401, error ? 'invalid_credentials' : 'unauthorized', error ?? 'Not authenticated');
      sendJson(res, 200, describePrincipal(principal));
      return;
    }

    case '/auth/token': {
      if (method !== 'POST') throw methodNotAllowed(['POST']);
      const { principal, error } = bc.auth();
      if (!principal) throw new HttpError(401, error ? 'invalid_credentials' : 'unauthorized', error ?? 'Not authenticated');
      const { fields } = await readFields(bc.req);
      let ttlMs = runtime.config.auth.tokenTtlMs;
      if (fields.ttl !== undefined) {
        try {
          ttlMs = typeof fields.ttl === 'number' ? fields.ttl * 1000 : parseDuration(String(fields.ttl));
        } catch (err) {
          throw new HttpError(400, 'invalid_ttl', (err as Error).message);
        }
        if (!(ttlMs > 0)) throw new HttpError(400, 'invalid_ttl', 'ttl must be positive');
      }
      // A token can never outlive the token it was minted from.
      if (principal.tokenExpiresAt) ttlMs = Math.min(ttlMs, principal.tokenExpiresAt - Date.now());
      const issued = auth.issueToken(principal, ttlMs);
      sendJson(res, 200, { token: issued.token, token_type: 'Bearer', expires_at: new Date(issued.expiresAt).toISOString() });
      return;
    }

    default:
      sendError(res, 404, 'not_found', 'Unknown auth endpoint');
  }
}
