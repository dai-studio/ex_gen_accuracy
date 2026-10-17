import { isAdmin } from '../auth.ts';
import { HttpError, redirect, sendError, sendJson } from '../util.ts';
import { methodNotAllowed, sendPage, type BuiltinContext } from './common.ts';

/**
 * /admin                    dashboard page (admin role)
 * /admin/api/status         gateway status
 * /admin/api/routes         routes, middleware and upstream health
 * /admin/api/stats          request and token counters
 * /admin/api/stats/reset    POST: reset counters
 * /admin/api/reload         POST: reload the configuration file
 */
export async function adminRoutes(bc: BuiltinContext): Promise<void> {
  const { res, method, gateway, runtime, path } = bc;
  const { principal, error } = bc.auth();

  if (path === '/admin' || path === '/admin/') {
    if (method !== 'GET' && method !== 'HEAD') throw methodNotAllowed(['GET']);
    if (!principal) {
      if (runtime.config.builtins.auth) redirect(res, `/auth/login?next=${encodeURIComponent('/admin')}`);
      else sendError(res, 401, 'unauthorized', 'Authentication required');
      return;
    }
    if (!isAdmin(principal)) {
      sendError(res, 403, 'forbidden', 'The admin role is required');
      return;
    }
    sendPage(res, 'admin', { title: runtime.config.builtins.title, user: principal.name });
    return;
  }

  if (!principal) {
    throw new HttpError(401, error ? 'invalid_credentials' : 'unauthorized', error ?? 'Authentication required', {
      'www-authenticate': 'Bearer realm="moxy"',
    });
  }
  if (!isAdmin(principal)) throw new HttpError(403, 'forbidden', 'The admin role is required');

  switch (path) {
    case '/admin/api/status':
      if (method !== 'GET') throw methodNotAllowed(['GET']);
      sendJson(res, 200, gateway.status());
      return;

    case '/admin/api/routes':
      if (method !== 'GET') throw methodNotAllowed(['GET']);
      sendJson(res, 200, {
        routes: runtime.routes.map((r) => ({
          id: r.id,
          path: r.path,
          methods: r.methods ?? ['*'],
          description: r.description,
          hidden: r.hidden,
          strategy: r.strategy,
          retries: r.retries,
          retry_on: r.retryOn,
          middleware: r.chain.map((m) => m.type),
          models: r.models?.models.map((m) => ({ id: m.id, upstream_model: m.upstreamModel ?? m.id })) ?? [],
          upstreams: r.upstreams.map((u) => ({
            name: u.name,
            url: u.url,
            weight: u.weight,
            timeout_ms: u.timeoutMs,
            // Header values usually carry credentials; only names are exposed.
            injected_headers: u.headers.map(([name]) => name),
            health: gateway.health.get(u.url),
          })),
        })),
      });
      return;

    case '/admin/api/stats':
      if (method !== 'GET') throw methodNotAllowed(['GET']);
      sendJson(res, 200, gateway.stats.snapshot());
      return;

    case '/admin/api/stats/reset':
      if (method !== 'POST') throw methodNotAllowed(['POST']);
      gateway.stats.reset();
      gateway.logger.info('stats reset', { by: principal.id });
      sendJson(res, 200, { ok: true });
      return;

    case '/admin/api/reload': {
      if (method !== 'POST') throw methodNotAllowed(['POST']);
      const result = gateway.reload();
      gateway.logger.info('reload requested', { by: principal.id, ok: result.ok });
      sendJson(res, result.ok ? 200 : 422, result);
      return;
    }

    default:
      sendError(res, 404, 'not_found', 'Unknown admin endpoint');
  }
}
