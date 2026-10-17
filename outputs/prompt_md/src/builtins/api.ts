import { Context, flattenHeaders } from '../context.ts';
import { runPipeline } from '../pipeline.ts';
import { routeAllows } from '../runtime.ts';
import { HttpError, readBody, sendError, sendJson } from '../util.ts';
import { methodNotAllowed, type BuiltinContext } from './common.ts';

/**
 * OpenAI-compatible unified API. Requests are dispatched by "model" to the
 * route that declares it in <models>, and run through that route's middleware.
 *
 * GET  /api/v1/models
 * GET  /api/v1/models/{id}
 * POST /api/v1/chat/completions
 * POST /api/v1/embeddings
 */
export async function apiRoutes(bc: BuiltinContext): Promise<void> {
  const { res, method, path, runtime } = bc;

  if (path === '/api' || path === '/api/' || path === '/api/v1' || path === '/api/v1/') {
    sendJson(res, 200, {
      object: 'moxy.api',
      endpoints: {
        models: '/api/v1/models',
        chat_completions: '/api/v1/chat/completions',
        embeddings: '/api/v1/embeddings',
      },
    });
    return;
  }

  if (path === '/api/v1/models') {
    if (method !== 'GET' && method !== 'HEAD') throw methodNotAllowed(['GET']);
    sendJson(res, 200, { object: 'list', data: listModels(bc) });
    return;
  }

  if (path.startsWith('/api/v1/models/')) {
    if (method !== 'GET' && method !== 'HEAD') throw methodNotAllowed(['GET']);
    const id = decodeURIComponent(path.slice('/api/v1/models/'.length));
    const model = listModels(bc).find((m) => m.id === id);
    if (!model) throw new HttpError(404, 'model_not_found', `Model "${id}" does not exist or is not available to you`);
    sendJson(res, 200, model);
    return;
  }

  if (path === '/api/v1/chat/completions' || path === '/api/v1/embeddings') {
    if (method !== 'POST') throw methodNotAllowed(['POST']);
    await dispatch(bc, path.endsWith('/embeddings') ? 'embeddings' : 'chat');
    return;
  }

  sendError(res, 404, 'not_found', 'Unknown API endpoint');
}

function listModels(bc: BuiltinContext) {
  const principal = bc.principal();
  const created = Math.floor(bc.gateway.startedAt / 1000);
  const out = [];
  for (const [id, { route, model }] of bc.runtime.models) {
    if (route.hidden || !routeAllows(route, principal)) continue;
    out.push({ id, object: 'model', created, owned_by: route.id, ...(model.description ? { description: model.description } : {}) });
  }
  return out;
}

async function dispatch(bc: BuiltinContext, kind: 'chat' | 'embeddings'): Promise<void> {
  const { req, runtime } = bc;
  const body = await readBody(req, runtime.config.server.maxBodyBytes);
  let json: any;
  try {
    json = JSON.parse(body.toString('utf8'));
  } catch {
    throw new HttpError(400, 'invalid_json', 'Request body must be JSON');
  }
  if (!json || typeof json !== 'object' || Array.isArray(json)) throw new HttpError(400, 'invalid_json', 'Request body must be a JSON object');
  if (typeof json.model !== 'string' || !json.model) throw new HttpError(400, 'model_required', 'A "model" field is required');
  const entry = runtime.models.get(json.model);
  if (!entry || entry.route.hidden) {
    throw new HttpError(404, 'model_not_found', `Model "${json.model}" is not served by this gateway (see /api/v1/models)`);
  }
  const models = entry.route.models!;
  const headers = flattenHeaders(req.headers);
  headers['content-type'] = 'application/json';
  const ctx = new Context({
    id: bc.id,
    req,
    route: entry.route,
    method: 'POST',
    path: bc.path,
    query: '',
    upstreamPath: kind === 'chat' ? models.chatPath : models.embeddingsPath,
    body,
    clientIp: bc.clientIp,
    auth: runtime.auth,
    headers,
  });
  bc.attach(ctx);
  await runPipeline(ctx, bc.res, bc.gateway.pipelineDeps(bc.secure));
}
