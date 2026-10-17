import { ConfigError, type MiddlewareDef } from '../config.ts';
import { auth, bodyLimit, cors, ipFilter, quota, rateLimit } from './access.ts';
import { cache } from './cache.ts';
import { jsonSet, maxTokens, modelFilter, redact, removeHeader, rewritePath, setHeader, systemPrompt, timeout } from './transform.ts';
import type { Middleware, MiddlewareEnv, MiddlewareFactory } from './types.ts';

export const MIDDLEWARE: Record<string, MiddlewareFactory> = {
  auth,
  'ip-filter': ipFilter,
  'rate-limit': rateLimit,
  quota,
  'body-limit': bodyLimit,
  cors,
  'set-header': setHeader,
  'remove-header': removeHeader,
  'rewrite-path': rewritePath,
  timeout,
  'json-set': jsonSet,
  'max-tokens': maxTokens,
  'model-filter': modelFilter,
  'system-prompt': systemPrompt,
  redact,
  cache,
};

export function createMiddleware(def: MiddlewareDef, env: MiddlewareEnv): Middleware {
  const factory = Object.hasOwn(MIDDLEWARE, def.type) ? MIDDLEWARE[def.type] : undefined;
  if (!factory) {
    throw new ConfigError(`Unknown middleware <${def.type}> (available: ${Object.keys(MIDDLEWARE).join(', ')}, use)`, def.element);
  }
  if (def.type !== 'redact' && def.element.children.length) {
    throw new ConfigError(`<${def.type}> does not take child elements`, def.element.children[0]);
  }
  return factory(def, env);
}
