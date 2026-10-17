export interface Routable {
  path: string;
  methods: string[] | null;
}

export interface RouteMatch<R> {
  route: R;
  /** Path below a prefix route's base ("" for exact routes). Always "" or starting with "/". */
  remainder: string;
}

export type MatchResult<R> =
  | { kind: 'match'; match: RouteMatch<R> }
  | { kind: 'method-not-allowed'; allowed: string[] }
  | { kind: 'none' };

interface Entry<R> {
  route: R;
  exact: boolean;
  base: string;
  order: number;
}

/**
 * Matches request paths against route patterns. Patterns are either exact
 * ("/v1/chat") or prefixes ("/openai/*", which also matches "/openai").
 * Exact routes beat prefix routes; longer prefixes beat shorter ones; ties go
 * to the route declared first. OPTIONS requests ignore method restrictions so
 * CORS preflights reach the route's middleware.
 */
export class Router<R extends Routable> {
  private readonly entries: Entry<R>[];

  constructor(routes: R[]) {
    this.entries = routes
      .map((route, order) => {
        const exact = !route.path.endsWith('/*');
        const base = exact ? route.path : route.path.slice(0, -2) || '/';
        return { route, exact, base, order };
      })
      .sort((a, b) => {
        if (a.exact !== b.exact) return a.exact ? -1 : 1;
        if (a.base.length !== b.base.length) return b.base.length - a.base.length;
        return a.order - b.order;
      });
  }

  match(method: string, path: string): MatchResult<R> {
    const allowed = new Set<string>();
    let pathMatched = false;
    for (const e of this.entries) {
      const remainder = matchPath(e, path);
      if (remainder === null) continue;
      pathMatched = true;
      const methods = e.route.methods;
      if (!methods || method === 'OPTIONS' || methods.includes(method) || (method === 'HEAD' && methods.includes('GET'))) {
        return { kind: 'match', match: { route: e.route, remainder } };
      }
      for (const m of methods) allowed.add(m);
    }
    return pathMatched ? { kind: 'method-not-allowed', allowed: [...allowed] } : { kind: 'none' };
  }
}

function matchPath<R>(e: Entry<R>, path: string): string | null {
  if (e.exact) return path === e.base ? '' : null;
  if (e.base === '/') return path;
  if (path === e.base) return '';
  if (path.startsWith(`${e.base}/`)) return path.slice(e.base.length);
  return null;
}
