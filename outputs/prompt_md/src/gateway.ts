import { randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import http, { type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import https from 'node:https';
import type { AddressInfo, Socket } from 'node:net';
import type { TLSSocket } from 'node:tls';
import { RevocationList } from './auth.ts';
import { adminRoutes } from './builtins/admin.ts';
import { apiRoutes } from './builtins/api.ts';
import { authRoutes } from './builtins/auth.ts';
import type { BuiltinContext, BuiltinHandler } from './builtins/common.ts';
import { chatRoutes, discovery, health, indexPage } from './builtins/pages.ts';
import { BUILTIN_PREFIXES, HEALTH_PATH, loadConfigFile, within, type MoxyConfig } from './config.ts';
import { Context } from './context.ts';
import { Logger } from './logger.ts';
import { SlidingWindowLimiter } from './middleware/helpers.ts';
import { runPipeline, type PipelineDeps } from './pipeline.ts';
import { UpstreamHealth } from './proxy.ts';
import { buildRuntime, type Runtime } from './runtime.ts';
import { Stats } from './stats.ts';
import { HttpError, randomId, readBody, sendError } from './util.ts';
import { VERSION } from './version.ts';

export interface GatewayOptions {
  config: MoxyConfig;
  /** Defaults to config.sourcePath; required for reloads. */
  configPath?: string;
  logger?: Logger;
}

export type ReloadResult = { ok: true; routes: number; loaded_at: string; warnings: string[] } | { ok: false; error: string };

const REQUEST_ID = /^[A-Za-z0-9._:-]{1,128}$/;

/** True if any path segment decodes to "." or ".." (guards upstream path traversal). */
export function hasDotSegments(path: string): boolean {
  return path.split('/').some((seg) => {
    let decoded: string;
    try {
      decoded = decodeURIComponent(seg);
    } catch {
      return true;
    }
    return decoded.split(/[\\/]/).some((part) => part === '..' || part === '.');
  });
}

export class Gateway {
  readonly logger: Logger;
  readonly stats = new Stats();
  readonly health = new UpstreamHealth();
  readonly revocations = new RevocationList();
  readonly startedAt = Date.now();
  readonly configPath?: string;
  runtime: Runtime;
  loadedAt = new Date();
  private server?: Server;
  private readonly sockets = new Set<Socket>();
  private generatedSecret?: string;
  private readonly ownsLogger: boolean;
  private readonly loginLimiter = new SlidingWindowLimiter(10, 5 * 60_000);

  constructor(options: GatewayOptions) {
    this.configPath = options.configPath ?? options.config.sourcePath;
    this.ownsLogger = !options.logger;
    this.logger = options.logger ?? new Logger(options.config.logging.level, options.config.logging.format);
    this.runtime = this.build(options.config);
  }

  get config(): MoxyConfig {
    return this.runtime.config;
  }

  private build(config: MoxyConfig): Runtime {
    if (!config.auth.secret && !this.generatedSecret) {
      this.generatedSecret = randomBytes(32).toString('base64url');
      if (config.auth.users.length || config.auth.apiKeys.length) {
        this.logger.warn('no auth secret configured; generated a random one, so tokens will not survive restarts');
      }
    }
    const runtime = buildRuntime(config, {
      logger: this.logger,
      secret: config.auth.secret || this.generatedSecret!,
      revocations: this.revocations,
    });
    if (config.builtins.admin && !config.auth.users.some((u) => u.roles.includes('admin')) && !config.auth.apiKeys.some((k) => k.roles.includes('admin'))) {
      this.logger.warn('admin built-in is enabled but no user or api-key has the admin role');
    }
    return runtime;
  }

  /** Re-reads the configuration file. On failure the running configuration is kept. */
  reload(): ReloadResult {
    if (!this.configPath) return { ok: false, error: 'Gateway was not started from a config file' };
    try {
      const config = loadConfigFile(this.configPath);
      const runtime = this.build(config);
      const warnings: string[] = [];
      const prev = this.runtime.config.server;
      if (config.server.port !== prev.port || config.server.host !== prev.host || !!config.server.tls !== !!prev.tls) {
        warnings.push('server host/port/TLS changes take effect after a restart');
      }
      this.runtime = runtime;
      this.loadedAt = new Date();
      if (this.ownsLogger) {
        this.logger.level = config.logging.level;
        this.logger.format = config.logging.format;
      }
      this.logger.info('configuration reloaded', { routes: runtime.routes.length, path: this.configPath });
      return { ok: true, routes: runtime.routes.length, loaded_at: this.loadedAt.toISOString(), warnings };
    } catch (err) {
      const error = (err as Error).message;
      this.logger.error('configuration reload failed; keeping previous configuration', { error });
      return { ok: false, error };
    }
  }

  /** Brute-force protection: an IP with too many recent failed logins is turned away. */
  loginBlocked(ip: string): boolean {
    return !this.loginLimiter.wouldAllow(ip);
  }

  recordLoginFailure(ip: string): void {
    this.loginLimiter.hit(ip);
  }

  pipelineDeps(secure: boolean): PipelineDeps {
    return { health: this.health, logger: this.logger, cookieName: this.runtime.auth.cookieName, protocol: secure ? 'https' : 'http' };
  }

  status(): Record<string, unknown> {
    const address = this.server?.address() as AddressInfo | null | undefined;
    const cfg = this.runtime.config;
    return {
      name: 'moxy',
      version: VERSION,
      started_at: new Date(this.startedAt).toISOString(),
      uptime_s: Math.floor((Date.now() - this.startedAt) / 1000),
      config_path: this.configPath ?? null,
      config_loaded_at: this.loadedAt.toISOString(),
      listening: address ? { host: address.address, port: address.port, tls: !!cfg.server.tls } : null,
      routes: this.runtime.routes.length,
      models: this.runtime.models.size,
      builtins: cfg.builtins,
      auth: {
        users: cfg.auth.users.length,
        api_keys: cfg.auth.apiKeys.length,
        secret: cfg.auth.secret ? 'configured' : 'generated',
      },
      node: process.version,
    };
  }

  readonly handler = (req: IncomingMessage, res: ServerResponse): void => {
    void this.handle(req, res);
  };

  async listen(port?: number, host?: string): Promise<AddressInfo> {
    const cfg = this.runtime.config.server;
    const server = cfg.tls
      ? https.createServer({ cert: readFileSync(cfg.tls.cert), key: readFileSync(cfg.tls.key) }, this.handler)
      : http.createServer(this.handler);
    // Bounds how long a client may take to *send* its request (slow-body protection).
    // It does not limit how long a streamed model response may run.
    server.requestTimeout = 120_000;
    server.on('connection', (socket: Socket) => {
      this.sockets.add(socket);
      socket.on('close', () => this.sockets.delete(socket));
    });
    server.on('clientError', (_err, socket) => {
      if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n');
      else socket.destroy();
    });
    this.server = server;
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(port ?? cfg.port, host ?? cfg.host, () => {
        server.off('error', reject);
        resolve();
      });
    });
    const address = server.address() as AddressInfo;
    this.logger.info('moxy listening', {
      url: `${cfg.tls ? 'https' : 'http'}://${address.family === 'IPv6' ? `[${address.address}]` : address.address}:${address.port}`,
      routes: this.runtime.routes.length,
      version: VERSION,
    });
    return address;
  }

  /** Stops accepting connections; open connections get `graceMs` to finish. */
  async close(graceMs = 5000): Promise<void> {
    const server = this.server;
    if (!server) return;
    this.server = undefined;
    const closed = new Promise<void>((resolve) => server.close(() => resolve()));
    server.closeIdleConnections();
    const timer = setTimeout(() => {
      for (const s of this.sockets) s.destroy();
    }, graceMs);
    timer.unref();
    await closed;
    clearTimeout(timer);
  }

  clientIp(req: IncomingMessage, trustProxy: boolean): string {
    if (trustProxy) {
      const xff = req.headers['x-forwarded-for'];
      const first = (Array.isArray(xff) ? xff[0] : xff)?.split(',')[0]?.trim();
      if (first) return first;
    }
    return req.socket.remoteAddress ?? 'unknown';
  }

  private isSecure(req: IncomingMessage, trustProxy: boolean): boolean {
    if ((req.socket as TLSSocket).encrypted) return true;
    return trustProxy && String(req.headers['x-forwarded-proto'] ?? '').split(',')[0]?.trim() === 'https';
  }

  private builtinFor(rt: Runtime, path: string): [string, BuiltinHandler] | null {
    const b = rt.config.builtins;
    if (path === HEALTH_PATH) return ['health', health];
    if (b.discovery && path === BUILTIN_PREFIXES.discovery) return ['discovery', discovery];
    if (b.admin && within(path, BUILTIN_PREFIXES.admin)) return ['admin', adminRoutes];
    if (b.auth && within(path, BUILTIN_PREFIXES.auth)) return ['auth', authRoutes];
    if (b.chat && within(path, BUILTIN_PREFIXES.chat)) return ['chat', chatRoutes];
    if (b.api && within(path, BUILTIN_PREFIXES.api)) return ['api', apiRoutes];
    return null;
  }

  private async handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const started = Date.now();
    const rt = this.runtime;
    const trustProxy = rt.config.server.trustProxy;
    const incomingId = req.headers['x-request-id'];
    const id = typeof incomingId === 'string' && REQUEST_ID.test(incomingId) ? incomingId : randomId();
    const clientIp = this.clientIp(req, trustProxy);
    const secure = this.isSecure(req, trustProxy);
    const method = (req.method ?? 'GET').toUpperCase();
    res.setHeader('x-request-id', id);

    let path = '';
    let builtin: string | undefined;
    let ctx: Context | undefined;
    let builtinPrincipal: string | undefined;

    try {
      if (!req.url?.startsWith('/')) throw new HttpError(400, 'bad_request', 'Request target must be an absolute path');
      // Checked on the raw target: URL parsing would silently resolve "..".
      if (hasDotSegments(req.url.split('?')[0]!)) throw new HttpError(400, 'bad_request', 'Path must not contain dot segments');
      let url: URL;
      try {
        url = new URL(req.url, 'http://moxy.invalid');
      } catch {
        throw new HttpError(400, 'bad_request', 'Malformed request URL');
      }
      path = url.pathname;

      const found = this.builtinFor(rt, path);
      if (found) {
        builtin = found[0];
        let authResult: ReturnType<Runtime['auth']['resolve']> | undefined;
        const bc: BuiltinContext = {
          req,
          res,
          url,
          path,
          method,
          id,
          clientIp,
          secure,
          gateway: this,
          runtime: rt,
          auth: () => {
            authResult ??= rt.auth.resolve(req);
            builtinPrincipal = authResult.principal?.id;
            return authResult;
          },
          principal: () => bc.auth().principal,
          attach: (c) => {
            ctx = c;
          },
        };
        await found[1](bc);
        return;
      }

      const match = rt.router.match(method, path);
      if (match.kind === 'none') {
        const b = rt.config.builtins;
        if (path === '/' && (method === 'GET' || method === 'HEAD') && (b.chat || b.admin || b.auth)) {
          builtin = 'index';
          indexPage({ req, res, url, path, method, id, clientIp, secure, gateway: this, runtime: rt, auth: () => ({ principal: null }), principal: () => null, attach: () => {} });
          return;
        }
        throw new HttpError(404, 'route_not_found', `No route matches ${path}`);
      }
      if (match.kind === 'method-not-allowed') {
        throw new HttpError(405, 'method_not_allowed', `${method} is not allowed on ${path}`, { allow: match.allowed.join(', ') });
      }

      const body = await readBody(req, rt.config.server.maxBodyBytes);
      ctx = new Context({
        id,
        req,
        route: match.match.route,
        path,
        query: url.search,
        upstreamPath: match.match.remainder,
        body,
        clientIp,
        auth: rt.auth,
      });
      await runPipeline(ctx, res, this.pipelineDeps(secure));
    } catch (err) {
      if (err instanceof HttpError) {
        if (!res.headersSent) sendError(res, err.status, err.code, err.message, err.headers);
        else res.destroy();
      } else {
        this.logger.error('unhandled error', { id, path, error: (err as Error).stack ?? String(err) });
        if (!res.headersSent) sendError(res, 500, 'internal_error', 'Internal gateway error');
        else res.destroy();
      }
    } finally {
      const latencyMs = Date.now() - started;
      const status = res.statusCode;
      const principalId = ctx ? ctx.principal?.id : builtinPrincipal;
      if (ctx) {
        this.stats.record({ routeId: ctx.route.id, principalId, model: ctx.model, status, latencyMs, usage: ctx.usage });
      }
      if (rt.config.logging.access) {
        this.logger.info('request', {
          id,
          method,
          path,
          status,
          ms: latencyMs,
          ip: clientIp,
          route: ctx?.route.id,
          builtin,
          principal: principalId,
          model: ctx?.model,
          upstream: ctx?.upstreamUrl,
          tokens: ctx?.usage?.totalTokens,
        });
      }
    }
  }
}
