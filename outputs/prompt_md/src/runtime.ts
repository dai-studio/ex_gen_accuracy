import { AuthService, type Principal, type RevocationList } from './auth.ts';
import type { ModelDef, MoxyConfig } from './config.ts';
import type { Logger } from './logger.ts';
import { createMiddleware } from './middleware/index.ts';
import type { CompiledRoute } from './middleware/types.ts';
import { Router } from './router.ts';

export interface ModelEntry {
  route: CompiledRoute;
  model: ModelDef;
}

/** Everything derived from one configuration; swapped atomically on reload. */
export interface Runtime {
  config: MoxyConfig;
  auth: AuthService;
  routes: CompiledRoute[];
  router: Router<CompiledRoute>;
  /** Public model id -> serving route (first declaration wins). */
  models: Map<string, ModelEntry>;
}

export interface RuntimeDeps {
  logger: Logger;
  secret: string;
  revocations: RevocationList;
}

export function buildRuntime(config: MoxyConfig, deps: RuntimeDeps): Runtime {
  const auth = new AuthService(config.auth, deps.secret, deps.revocations);
  const globalChain = config.globalMiddleware.map((def) => createMiddleware(def, { auth, logger: deps.logger, scope: 'global' }));
  const routes: CompiledRoute[] = config.routes.map((route) => ({
    ...route,
    chain: [...globalChain, ...route.middleware.map((def) => createMiddleware(def, { auth, logger: deps.logger, scope: route.id }))],
    cursor: 0,
  }));
  const models = new Map<string, ModelEntry>();
  for (const route of routes) {
    for (const model of route.models?.models ?? []) {
      const existing = models.get(model.id);
      if (existing) {
        deps.logger.warn('model served by multiple routes; the first one wins for /api/v1', {
          model: model.id,
          used: existing.route.id,
          ignored: route.id,
        });
        continue;
      }
      models.set(model.id, { route, model });
    }
  }
  return { config, auth, routes, router: new Router(routes), models };
}

/** Whether the caller would pass every access-control middleware on the route. */
export function routeAllows(route: CompiledRoute, principal: Principal | null): boolean {
  return route.chain.every((m) => !m.allows || m.allows(principal));
}

export function routeAuthInfo(route: CompiledRoute): { required: boolean; roles: string[] } {
  const roles = new Set<string>();
  let required = false;
  for (const m of route.chain) {
    if (m.type !== 'auth' || !m.info) continue;
    if (!m.info.optional) required = true;
    for (const r of (m.info.roles as string[]) ?? []) roles.add(r);
  }
  return { required, roles: [...roles] };
}
