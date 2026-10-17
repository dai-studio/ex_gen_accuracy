import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { parseXml, XmlError, type XmlElement } from './xml.ts';
import { parseBool, parseDuration, parseIntStrict, parseSize, splitList } from './util.ts';

export interface ServerConfig {
  host: string;
  port: number;
  /** Trust X-Forwarded-For / X-Forwarded-Proto from the immediate peer. */
  trustProxy: boolean;
  /** Hard cap on buffered request bodies. */
  maxBodyBytes: number;
  tls?: { cert: string; key: string };
}

export type LogLevel = 'debug' | 'info' | 'warn' | 'error' | 'silent';

export interface LoggingConfig {
  level: LogLevel;
  access: boolean;
  format: 'json' | 'text';
}

export interface UserDef {
  name: string;
  roles: string[];
  password?: string;
  passwordHash?: string;
}

export interface ApiKeyDef {
  name: string;
  roles: string[];
  /** Lowercase hex SHA-256 of the key. */
  keyHash: string;
}

export interface AuthConfig {
  /** Empty when not configured; the gateway then generates one per process. */
  secret: string;
  tokenTtlMs: number;
  cookieName: string;
  users: UserDef[];
  apiKeys: ApiKeyDef[];
}

export interface BuiltinsConfig {
  admin: boolean;
  auth: boolean;
  chat: boolean;
  api: boolean;
  discovery: boolean;
  /** Require a login for the chat UI (only effective when users exist). */
  chatAuth: boolean;
  title: string;
}

export interface UpstreamDef {
  name?: string;
  url: string;
  weight: number;
  timeoutMs?: number;
  headers: Array<[string, string]>;
}

export interface MiddlewareDef {
  type: string;
  attrs: Record<string, string>;
  element: XmlElement;
}

export interface ModelDef {
  id: string;
  upstreamModel?: string;
  description?: string;
}

export interface ModelsDef {
  chatPath: string;
  embeddingsPath: string;
  models: ModelDef[];
}

export type Strategy = 'failover' | 'round-robin' | 'random';

export interface RouteDef {
  id: string;
  path: string;
  /** Upper-case method names, or null for any method. */
  methods: string[] | null;
  description: string;
  hidden: boolean;
  preserveHost: boolean;
  strategy: Strategy;
  retries: number;
  /** Tokens: "error", "5xx", "4xx" or explicit status codes. */
  retryOn: string[];
  upstreams: UpstreamDef[];
  middleware: MiddlewareDef[];
  models?: ModelsDef;
  line: number;
}

export interface MoxyConfig {
  server: ServerConfig;
  logging: LoggingConfig;
  auth: AuthConfig;
  builtins: BuiltinsConfig;
  globalMiddleware: MiddlewareDef[];
  routes: RouteDef[];
  sourcePath?: string;
}

export class ConfigError extends Error {
  readonly line?: number;

  constructor(message: string, at?: { line: number }) {
    super(at ? `${message} (line ${at.line})` : message);
    this.name = 'ConfigError';
    this.line = at?.line;
  }
}

/** Paths under these prefixes belong to the built-in routes when enabled. */
export const BUILTIN_PREFIXES: Record<keyof Omit<BuiltinsConfig, 'chatAuth' | 'title'>, string> = {
  admin: '/admin',
  auth: '/auth',
  chat: '/chat',
  api: '/api',
  discovery: '/.well-known/moxy.json',
};
export const HEALTH_PATH = '/healthz';

export interface LoadOptions {
  env?: NodeJS.ProcessEnv;
  /** Used to resolve relative file paths (TLS certificates). */
  baseDir?: string;
}

export function loadConfigFile(path: string, env: NodeJS.ProcessEnv = process.env): MoxyConfig {
  const abs = resolve(path);
  let source: string;
  try {
    source = readFileSync(abs, 'utf8');
  } catch (err) {
    throw new ConfigError(`Cannot read config file ${abs}: ${(err as Error).message}`);
  }
  const config = parseConfig(source, { env, baseDir: dirname(abs) });
  config.sourcePath = abs;
  return config;
}

export function parseConfig(source: string, options: LoadOptions = {}): MoxyConfig {
  let root: XmlElement;
  try {
    root = parseXml(source);
  } catch (err) {
    if (err instanceof XmlError) throw new ConfigError(`Malformed XML: ${err.message}`);
    throw err;
  }
  interpolateTree(root, options.env ?? process.env);
  return new ConfigReader(options.baseDir ?? process.cwd()).read(root);
}

const ENV_REF = /\$\$\{|\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}/g;

/** Expands ${VAR} and ${VAR:-default}; "$${" yields a literal "${". */
export function interpolate(value: string, env: NodeJS.ProcessEnv, at?: { line: number }): string {
  return value.replace(ENV_REF, (match, name: string | undefined, fallback: string | undefined) => {
    if (match === '$${') return '${';
    const v = env[name!];
    if (v !== undefined && v !== '') return v;
    if (fallback !== undefined) return fallback;
    throw new ConfigError(`Environment variable ${name} is not set (use \${${name}:-default} to make it optional)`, at);
  });
}

function interpolateTree(el: XmlElement, env: NodeJS.ProcessEnv): void {
  for (const key of Object.keys(el.attrs)) el.attrs[key] = interpolate(el.attrs[key]!, env, el);
  if (el.text) el.text = interpolate(el.text, env, el);
  for (const child of el.children) interpolateTree(child, env);
}

class ConfigReader {
  private readonly baseDir: string;
  private chains = new Map<string, XmlElement>();
  private namedUpstreams = new Map<string, UpstreamDef>();

  constructor(baseDir: string) {
    this.baseDir = baseDir;
  }

  read(root: XmlElement): MoxyConfig {
    if (root.name !== 'moxy') throw new ConfigError(`Root element must be <moxy>, found <${root.name}>`, root);
    checkAttrs(root, ['version']);
    const sections = new Map<string, XmlElement>();
    const allowed = ['server', 'logging', 'auth', 'builtins', 'upstreams', 'chains', 'middleware', 'routes'];
    for (const child of root.children) {
      if (!allowed.includes(child.name)) {
        throw new ConfigError(`Unknown element <${child.name}> in <moxy> (expected one of: ${allowed.join(', ')})`, child);
      }
      if (sections.has(child.name)) throw new ConfigError(`<${child.name}> may only appear once`, child);
      sections.set(child.name, child);
    }

    const chainsEl = sections.get('chains');
    if (chainsEl) this.readChains(chainsEl);
    const upstreamsEl = sections.get('upstreams');
    if (upstreamsEl) this.readNamedUpstreams(upstreamsEl);

    const builtins = this.readBuiltins(sections.get('builtins'));
    const routes = this.readRoutes(sections.get('routes'), builtins);
    const globalEl = sections.get('middleware');
    if (globalEl) checkAttrs(globalEl, []);

    return {
      server: this.readServer(sections.get('server')),
      logging: this.readLogging(sections.get('logging')),
      auth: this.readAuth(sections.get('auth')),
      builtins,
      globalMiddleware: globalEl ? this.readMiddlewareList(globalEl, []) : [],
      routes,
    };
  }

  private readServer(el: XmlElement | undefined): ServerConfig {
    const server: ServerConfig = { host: '127.0.0.1', port: 8080, trustProxy: false, maxBodyBytes: 10 * 1024 * 1024 };
    if (!el) return server;
    checkAttrs(el, ['host', 'port', 'trust-proxy', 'max-body', 'tls-cert', 'tls-key']);
    const a = el.attrs;
    if (a.host) server.host = a.host;
    if (a.port !== undefined) {
      server.port = attempt(el, () => parseIntStrict(a.port!, 'port'));
      if (server.port > 65535) throw new ConfigError(`Invalid port ${server.port}`, el);
    }
    server.trustProxy = attempt(el, () => parseBool(a['trust-proxy'], false));
    if (a['max-body']) server.maxBodyBytes = attempt(el, () => parseSize(a['max-body']!));
    if (a['tls-cert'] || a['tls-key']) {
      if (!a['tls-cert'] || !a['tls-key']) throw new ConfigError('Both tls-cert and tls-key are required for TLS', el);
      server.tls = { cert: resolve(this.baseDir, a['tls-cert']), key: resolve(this.baseDir, a['tls-key']) };
    }
    noChildren(el);
    return server;
  }

  private readLogging(el: XmlElement | undefined): LoggingConfig {
    const logging: LoggingConfig = { level: 'info', access: true, format: 'json' };
    if (!el) return logging;
    checkAttrs(el, ['level', 'access', 'format']);
    if (el.attrs.level) {
      const level = el.attrs.level as LogLevel;
      if (!['debug', 'info', 'warn', 'error', 'silent'].includes(level)) {
        throw new ConfigError(`Invalid log level "${level}"`, el);
      }
      logging.level = level;
    }
    logging.access = attempt(el, () => parseBool(el.attrs.access, true));
    if (el.attrs.format) {
      if (el.attrs.format !== 'json' && el.attrs.format !== 'text') throw new ConfigError('Log format must be json or text', el);
      logging.format = el.attrs.format;
    }
    noChildren(el);
    return logging;
  }

  private readAuth(el: XmlElement | undefined): AuthConfig {
    const auth: AuthConfig = { secret: '', tokenTtlMs: 12 * 3_600_000, cookieName: 'moxy_session', users: [], apiKeys: [] };
    if (!el) return auth;
    checkAttrs(el, ['secret', 'token-ttl', 'cookie']);
    const a = el.attrs;
    if (a.secret) {
      if (a.secret.length < 16) throw new ConfigError('auth secret must be at least 16 characters', el);
      auth.secret = a.secret;
    }
    if (a['token-ttl']) auth.tokenTtlMs = attempt(el, () => parseDuration(a['token-ttl']!));
    if (a.cookie) {
      if (!/^[A-Za-z0-9_\-]+$/.test(a.cookie)) throw new ConfigError('Invalid cookie name', el);
      auth.cookieName = a.cookie;
    }
    const names = new Set<string>();
    for (const child of el.children) {
      if (child.name === 'user') {
        checkAttrs(child, ['name', 'password', 'password-hash', 'roles']);
        const name = required(child, 'name');
        if (names.has(`user:${name}`)) throw new ConfigError(`Duplicate user "${name}"`, child);
        names.add(`user:${name}`);
        const { password, 'password-hash': passwordHash } = child.attrs;
        if (!password === !passwordHash) throw new ConfigError(`User "${name}" needs exactly one of password or password-hash`, child);
        if (passwordHash && !/^scrypt\$\d+\$\d+\$\d+\$[A-Za-z0-9+/=_-]+\$[A-Za-z0-9+/=_-]+$/.test(passwordHash)) {
          throw new ConfigError(`User "${name}" has a malformed password-hash (generate one with "moxy hash-password")`, child);
        }
        auth.users.push({ name, roles: splitList(child.attrs.roles), password, passwordHash });
      } else if (child.name === 'api-key') {
        checkAttrs(child, ['name', 'key', 'key-hash', 'roles']);
        const name = required(child, 'name');
        if (names.has(`key:${name}`)) throw new ConfigError(`Duplicate api-key "${name}"`, child);
        names.add(`key:${name}`);
        const { key, 'key-hash': keyHash } = child.attrs;
        if (!key === !keyHash) throw new ConfigError(`api-key "${name}" needs exactly one of key or key-hash`, child);
        let hash: string;
        if (keyHash) {
          const m = /^sha256:([0-9a-fA-F]{64})$/.exec(keyHash);
          if (!m) throw new ConfigError(`api-key "${name}" key-hash must look like sha256:<64 hex chars>`, child);
          hash = m[1]!.toLowerCase();
        } else {
          if (key!.length < 16) throw new ConfigError(`api-key "${name}" must be at least 16 characters`, child);
          hash = sha256Hex(key!);
        }
        if (auth.apiKeys.some((k) => k.keyHash === hash)) throw new ConfigError(`api-key "${name}" duplicates another key`, child);
        auth.apiKeys.push({ name, roles: splitList(child.attrs.roles), keyHash: hash });
      } else {
        throw new ConfigError(`Unknown element <${child.name}> in <auth> (expected <user> or <api-key>)`, child);
      }
    }
    return auth;
  }

  private readBuiltins(el: XmlElement | undefined): BuiltinsConfig {
    const b: BuiltinsConfig = { admin: true, auth: true, chat: true, api: true, discovery: true, chatAuth: true, title: 'Moxy' };
    if (!el) return b;
    checkAttrs(el, ['admin', 'auth', 'chat', 'api', 'discovery', 'chat-auth', 'title']);
    b.admin = attempt(el, () => parseBool(el.attrs.admin, true));
    b.auth = attempt(el, () => parseBool(el.attrs.auth, true));
    b.chat = attempt(el, () => parseBool(el.attrs.chat, true));
    b.api = attempt(el, () => parseBool(el.attrs.api, true));
    b.discovery = attempt(el, () => parseBool(el.attrs.discovery, true));
    b.chatAuth = attempt(el, () => parseBool(el.attrs['chat-auth'], true));
    if (el.attrs.title) b.title = el.attrs.title;
    noChildren(el);
    return b;
  }

  private readChains(el: XmlElement): void {
    checkAttrs(el, []);
    for (const chain of el.children) {
      if (chain.name !== 'chain') throw new ConfigError(`Unknown element <${chain.name}> in <chains> (expected <chain>)`, chain);
      checkAttrs(chain, ['name']);
      const name = required(chain, 'name');
      if (this.chains.has(name)) throw new ConfigError(`Duplicate chain "${name}"`, chain);
      this.chains.set(name, chain);
    }
  }

  private readMiddlewareList(el: XmlElement, usingChains: string[]): MiddlewareDef[] {
    const out: MiddlewareDef[] = [];
    for (const child of el.children) {
      if (child.name === 'use') {
        checkAttrs(child, ['chain']);
        const name = required(child, 'chain');
        const chain = this.chains.get(name);
        if (!chain) throw new ConfigError(`Unknown chain "${name}"`, child);
        if (usingChains.includes(name)) throw new ConfigError(`Chain "${name}" includes itself`, child);
        out.push(...this.readMiddlewareList(chain, [...usingChains, name]));
      } else {
        out.push({ type: child.name, attrs: child.attrs, element: child });
      }
    }
    return out;
  }

  private readUpstream(el: XmlElement, requireName: boolean): UpstreamDef {
    checkAttrs(el, requireName ? ['name', 'url', 'weight', 'timeout'] : ['ref', 'url', 'weight', 'timeout']);
    let base: UpstreamDef | undefined;
    if (!requireName && el.attrs.ref) {
      base = this.namedUpstreams.get(el.attrs.ref);
      if (!base) throw new ConfigError(`Unknown upstream "${el.attrs.ref}"`, el);
      if (el.attrs.url) throw new ConfigError('An upstream may have ref or url, not both', el);
    }
    const url = el.attrs.url ?? base?.url;
    if (!url) throw new ConfigError(requireName ? '<upstream> requires a url' : '<upstream> requires a url or ref', el);
    let parsed: URL;
    try {
      parsed = new URL(url);
    } catch {
      throw new ConfigError(`Invalid upstream url "${url}"`, el);
    }
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
      throw new ConfigError(`Upstream url must be http or https: "${url}"`, el);
    }
    if (parsed.search || parsed.hash) throw new ConfigError(`Upstream url must not contain a query or fragment: "${url}"`, el);
    const headers: Array<[string, string]> = [...(base?.headers ?? [])];
    for (const child of el.children) {
      if (child.name !== 'header') throw new ConfigError(`Unknown element <${child.name}> in <upstream> (expected <header>)`, child);
      checkAttrs(child, ['name', 'value']);
      const name = required(child, 'name').toLowerCase();
      if (!/^[a-z0-9!#$%&'*+.^_`|~-]+$/.test(name)) throw new ConfigError(`Invalid header name "${name}"`, child);
      headers.push([name, child.attrs.value ?? child.text.trim()]);
    }
    return {
      name: requireName ? required(el, 'name') : base?.name,
      url,
      weight: el.attrs.weight ? attempt(el, () => parseIntStrict(el.attrs.weight!, 'weight', 1)) : (base?.weight ?? 1),
      timeoutMs: el.attrs.timeout ? attempt(el, () => parseDuration(el.attrs.timeout!)) : base?.timeoutMs,
      headers,
    };
  }

  private readNamedUpstreams(el: XmlElement): void {
    checkAttrs(el, []);
    for (const child of el.children) {
      if (child.name !== 'upstream') throw new ConfigError(`Unknown element <${child.name}> in <upstreams>`, child);
      const up = this.readUpstream(child, true);
      if (this.namedUpstreams.has(up.name!)) throw new ConfigError(`Duplicate upstream "${up.name}"`, child);
      this.namedUpstreams.set(up.name!, up);
    }
  }

  private readRoutes(el: XmlElement | undefined, builtins: BuiltinsConfig): RouteDef[] {
    if (!el) return [];
    checkAttrs(el, []);
    const routes: RouteDef[] = [];
    const ids = new Set<string>();
    el.children.forEach((child, index) => {
      if (child.name !== 'route') throw new ConfigError(`Unknown element <${child.name}> in <routes> (expected <route>)`, child);
      const route = this.readRoute(child, index, builtins);
      if (ids.has(route.id)) throw new ConfigError(`Duplicate route id "${route.id}"`, child);
      ids.add(route.id);
      routes.push(route);
    });
    return routes;
  }

  private readRoute(el: XmlElement, index: number, builtins: BuiltinsConfig): RouteDef {
    checkAttrs(el, ['id', 'path', 'methods', 'description', 'hidden', 'preserve-host', 'strategy', 'retries', 'retry-on']);
    const a = el.attrs;
    const path = required(el, 'path');
    validateRoutePath(path, el);
    const base = path.endsWith('/*') ? path.slice(0, -2) || '/' : path;
    for (const [name, prefix] of Object.entries(BUILTIN_PREFIXES)) {
      if (builtins[name as keyof typeof BUILTIN_PREFIXES] && within(base, prefix)) {
        throw new ConfigError(`Route path "${path}" collides with the built-in ${name} routes at ${prefix} (disable them in <builtins> to reuse the path)`, el);
      }
    }
    if (within(base, HEALTH_PATH)) throw new ConfigError(`Route path "${path}" collides with the built-in ${HEALTH_PATH}`, el);

    const methods = a.methods ? splitList(a.methods).map((m) => m.toUpperCase()) : null;
    if (methods?.includes('*')) methods.length = 0;
    const strategy = (a.strategy ?? 'failover') as Strategy;
    if (!['failover', 'round-robin', 'random'].includes(strategy)) {
      throw new ConfigError(`Invalid strategy "${strategy}" (use failover, round-robin or random)`, el);
    }

    let description = a.description ?? '';
    const upstreams: UpstreamDef[] = [];
    let middleware: MiddlewareDef[] = [];
    let models: ModelsDef | undefined;
    const seen = new Set<string>();
    for (const child of el.children) {
      if (child.name !== 'upstream' && seen.has(child.name)) throw new ConfigError(`<${child.name}> may only appear once per route`, child);
      seen.add(child.name);
      switch (child.name) {
        case 'upstream':
          upstreams.push(this.readUpstream(child, false));
          break;
        case 'description':
          checkAttrs(child, []);
          description = child.text.trim().replace(/\s+/g, ' ');
          break;
        case 'middleware':
          checkAttrs(child, []);
          middleware = this.readMiddlewareList(child, []);
          break;
        case 'models':
          models = readModels(child);
          break;
        default:
          throw new ConfigError(`Unknown element <${child.name}> in <route> (expected upstream, description, middleware or models)`, child);
      }
    }
    if (upstreams.length === 0) throw new ConfigError(`Route "${a.id ?? path}" needs at least one <upstream>`, el);

    const retryOn = splitList(a['retry-on'] ?? 'error,502,503,504').map((t) => t.toLowerCase());
    for (const token of retryOn) {
      if (!['error', '5xx', '4xx'].includes(token) && !/^[1-5]\d\d$/.test(token)) {
        throw new ConfigError(`Invalid retry-on value "${token}" (use error, 5xx, 4xx or status codes)`, el);
      }
    }

    return {
      id: a.id ?? `route-${index + 1}`,
      path,
      methods: methods && methods.length ? methods : null,
      description,
      hidden: attempt(el, () => parseBool(a.hidden, false)),
      preserveHost: attempt(el, () => parseBool(a['preserve-host'], false)),
      strategy,
      retries: a.retries !== undefined ? attempt(el, () => parseIntStrict(a.retries!, 'retries')) : upstreams.length - 1,
      retryOn,
      upstreams,
      middleware,
      models,
      line: el.line,
    };
  }
}

function readModels(el: XmlElement): ModelsDef {
  checkAttrs(el, ['chat-path', 'embeddings-path']);
  const def: ModelsDef = {
    chatPath: el.attrs['chat-path'] ?? '/chat/completions',
    embeddingsPath: el.attrs['embeddings-path'] ?? '/embeddings',
    models: [],
  };
  for (const p of [def.chatPath, def.embeddingsPath]) {
    if (!p.startsWith('/')) throw new ConfigError(`Model API paths must start with "/": "${p}"`, el);
  }
  const ids = new Set<string>();
  for (const child of el.children) {
    if (child.name !== 'model') throw new ConfigError(`Unknown element <${child.name}> in <models> (expected <model>)`, child);
    checkAttrs(child, ['id', 'upstream-model', 'description']);
    const id = required(child, 'id');
    if (ids.has(id)) throw new ConfigError(`Duplicate model "${id}" in route`, child);
    ids.add(id);
    def.models.push({
      id,
      upstreamModel: child.attrs['upstream-model'],
      description: child.attrs.description ?? (child.text.trim() || undefined),
    });
  }
  return def;
}

function validateRoutePath(path: string, el: XmlElement): void {
  if (!path.startsWith('/')) throw new ConfigError(`Route path must start with "/": "${path}"`, el);
  const star = path.indexOf('*');
  if (star >= 0 && (star !== path.length - 1 || !path.endsWith('/*'))) {
    throw new ConfigError(`"*" is only allowed as a trailing "/*" in route paths: "${path}"`, el);
  }
  if (/[?#\s]/.test(path)) throw new ConfigError(`Route path must not contain whitespace, "?" or "#": "${path}"`, el);
  if (path.split('/').some((seg) => seg === '..' || seg === '.')) throw new ConfigError(`Route path must not contain dot segments: "${path}"`, el);
}

/** True if `path` equals `prefix` or lies underneath it. */
export function within(path: string, prefix: string): boolean {
  return path === prefix || path.startsWith(prefix.endsWith('/') ? prefix : `${prefix}/`);
}

export function checkAttrs(el: XmlElement, allowed: string[]): void {
  for (const name of Object.keys(el.attrs)) {
    if (!allowed.includes(name)) {
      const hint = allowed.length ? ` (allowed: ${allowed.join(', ')})` : '';
      throw new ConfigError(`Unknown attribute "${name}" on <${el.name}>${hint}`, el);
    }
  }
}

function noChildren(el: XmlElement): void {
  if (el.children.length) throw new ConfigError(`<${el.name}> does not take child elements`, el.children[0]);
}

function required(el: XmlElement, name: string): string {
  const v = el.attrs[name];
  if (v === undefined || v.trim() === '') throw new ConfigError(`<${el.name}> requires a "${name}" attribute`, el);
  return v.trim();
}

/** Runs a value parser, re-throwing failures as ConfigErrors pointing at `el`. */
export function attempt<T>(el: XmlElement, fn: () => T): T {
  try {
    return fn();
  } catch (err) {
    if (err instanceof ConfigError) throw err;
    throw new ConfigError(`<${el.name}>: ${(err as Error).message}`, el);
  }
}

export function sha256Hex(value: string): string {
  return createHash('sha256').update(value).digest('hex');
}
