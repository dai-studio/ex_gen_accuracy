import { BUILTIN_PREFIXES, HEALTH_PATH } from '../config.ts';
import { routeAllows, routeAuthInfo } from '../runtime.ts';
import { redirect, sendError, sendJson } from '../util.ts';
import { VERSION } from '../version.ts';
import { methodNotAllowed, sendPage, type BuiltinContext } from './common.ts';

/** GET /chat — browser chat UI backed by /api/v1. */
export function chatRoutes(bc: BuiltinContext): void {
  const { res, runtime, path, method } = bc;
  if (path !== '/chat' && path !== '/chat/') {
    sendError(res, 404, 'not_found', 'Not found');
    return;
  }
  if (method !== 'GET' && method !== 'HEAD') throw methodNotAllowed(['GET']);
  const b = runtime.config.builtins;
  const loginRequired = b.chatAuth && b.auth && runtime.auth.hasUsers;
  const principal = bc.principal();
  if (loginRequired && !principal) {
    redirect(res, `/auth/login?next=${encodeURIComponent('/chat')}`);
    return;
  }
  sendPage(res, 'chat', {
    title: b.title,
    user: principal?.name ?? '',
    canLogout: principal?.via === 'cookie' ? 'true' : 'false',
    apiEnabled: b.api ? 'true' : 'false',
    adminLink: b.admin && principal?.roles.includes('admin') ? 'true' : 'false',
  });
}

/** GET / — landing page linking to the enabled built-ins (only when no route claims "/"). */
export function indexPage(bc: BuiltinContext): void {
  const b = bc.runtime.config.builtins;
  const links: string[] = [];
  if (b.chat) links.push('<li><a href="/chat">Chat</a> — talk to the configured models</li>');
  if (b.admin) links.push('<li><a href="/admin">Admin</a> — routes, upstream health and usage</li>');
  if (b.auth) links.push('<li><a href="/auth/login">Sign in</a></li>');
  if (b.discovery) links.push('<li><a href="/.well-known/moxy.json">Endpoint discovery</a> (JSON)</li>');
  if (b.api) links.push('<li><a href="/api/v1/models">API</a> — OpenAI-compatible at <code>/api/v1</code></li>');
  sendPage(bc.res, 'index', { title: b.title, version: VERSION, links: links.join('\n') });
}

export function health(bc: BuiltinContext): void {
  sendJson(bc.res, 200, { status: 'ok', version: VERSION, uptime_s: Math.floor((Date.now() - bc.gateway.startedAt) / 1000) });
}

/**
 * GET /.well-known/moxy.json — machine-readable description of the gateway
 * for developers and AI agents: built-in endpoints, auth schemes, and the
 * (non-hidden) routes with their models and access requirements.
 */
export function discovery(bc: BuiltinContext): void {
  const { runtime, req } = bc;
  if (bc.method !== 'GET' && bc.method !== 'HEAD') throw methodNotAllowed(['GET']);
  const b = runtime.config.builtins;
  const principal = bc.principal();
  const host = req.headers.host ?? `${runtime.config.server.host}:${runtime.config.server.port}`;
  const endpoints: Record<string, unknown> = { discovery: BUILTIN_PREFIXES.discovery, health: HEALTH_PATH };
  if (b.auth) {
    Object.assign(endpoints, { login: '/auth/login', logout: '/auth/logout', me: '/auth/me', token: '/auth/token' });
  }
  if (b.chat) endpoints.chat_ui = '/chat';
  if (b.admin) endpoints.admin = '/admin';
  if (b.api) {
    endpoints.api = {
      openai_base_url: '/api/v1',
      models: '/api/v1/models',
      chat_completions: '/api/v1/chat/completions',
      embeddings: '/api/v1/embeddings',
    };
  }
  sendJson(bc.res, 200, {
    name: 'moxy',
    title: b.title,
    version: VERSION,
    base_url: `${bc.secure ? 'https' : 'http'}://${host}`,
    endpoints,
    authentication: {
      enabled: runtime.auth.enabled,
      schemes: [
        { type: 'bearer', header: 'Authorization', description: 'Moxy token (from /auth/login or /auth/token) or API key' },
        { type: 'api-key', header: 'X-API-Key' },
        { type: 'cookie', name: runtime.auth.cookieName, description: 'Browser session set by /auth/login' },
      ],
    },
    caller: principal ? { id: principal.id, roles: principal.roles } : null,
    routes: runtime.routes
      .filter((r) => !r.hidden)
      .map((r) => ({
        id: r.id,
        path: r.path,
        methods: r.methods ?? ['*'],
        description: r.description || undefined,
        auth: routeAuthInfo(r),
        accessible: routeAllows(r, principal),
        models: r.models?.models.map((m) => m.id) ?? [],
      })),
  });
}
