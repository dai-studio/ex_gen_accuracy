import { createHash, createHmac, randomBytes, scrypt, timingSafeEqual } from 'node:crypto';
import type { IncomingMessage } from 'node:http';
import type { AuthConfig, UserDef } from './config.ts';
import { parseCookies, randomId } from './util.ts';

export interface Principal {
  /** "user:<name>" or "key:<name>". */
  id: string;
  name: string;
  kind: 'user' | 'key';
  roles: string[];
  via: 'bearer' | 'api-key' | 'cookie';
  /** Present when authenticated by a Moxy token. */
  tokenId?: string;
  tokenExpiresAt?: number;
}

export interface AuthResult {
  principal: Principal | null;
  /** Set when a credential was presented but rejected. */
  error?: string;
}

export interface IssuedToken {
  token: string;
  expiresAt: number;
  id: string;
}

interface TokenPayload {
  sub: string;
  iat: number;
  exp: number;
  jti: string;
}

const SCRYPT = { N: 16384, r: 8, p: 1, keylen: 32 };
const ADMIN_ROLE = 'admin';

function scryptAsync(password: string, salt: Buffer, keylen: number, opts: { N: number; r: number; p: number }): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    scrypt(password, salt, keylen, { ...opts, maxmem: 128 * opts.N * opts.r * 2 }, (err, key) => (err ? reject(err) : resolve(key)));
  });
}

/** Produces "scrypt$N$r$p$<salt b64>$<hash b64>". */
export async function hashPassword(password: string): Promise<string> {
  const salt = randomBytes(16);
  const hash = await scryptAsync(password, salt, SCRYPT.keylen, SCRYPT);
  return ['scrypt', SCRYPT.N, SCRYPT.r, SCRYPT.p, salt.toString('base64'), hash.toString('base64')].join('$');
}

export async function verifyPassword(password: string, stored: string): Promise<boolean> {
  const parts = stored.split('$');
  if (parts.length !== 6 || parts[0] !== 'scrypt') return false;
  const [N, r, p] = parts.slice(1, 4).map(Number) as [number, number, number];
  const salt = Buffer.from(parts[4]!, 'base64');
  const expected = Buffer.from(parts[5]!, 'base64');
  if (!N || !r || !p || expected.length === 0) return false;
  const actual = await scryptAsync(password, salt, expected.length, { N, r, p });
  return timingSafeEqual(actual, expected);
}

export function hashApiKey(key: string): string {
  return createHash('sha256').update(key).digest('hex');
}

export function generateApiKey(): string {
  return `moxy_${randomBytes(24).toString('base64url')}`;
}

function digestEqual(a: string, b: string): boolean {
  return timingSafeEqual(createHash('sha256').update(a).digest(), createHash('sha256').update(b).digest());
}

/** Empty `roles` means "any authenticated principal"; the admin role passes every check. */
export function hasRole(principal: Principal, roles: string[]): boolean {
  if (roles.length === 0 || principal.roles.includes(ADMIN_ROLE)) return true;
  return roles.some((r) => principal.roles.includes(r));
}

export function isAdmin(principal: Principal | null): boolean {
  return !!principal && principal.roles.includes(ADMIN_ROLE);
}

/** Tracks logged-out token ids until they would have expired anyway. */
export class RevocationList {
  private readonly revoked = new Map<string, number>();

  revoke(id: string, expiresAt: number): void {
    const now = Date.now();
    for (const [k, exp] of this.revoked) if (exp < now) this.revoked.delete(k);
    this.revoked.set(id, expiresAt);
  }

  has(id: string): boolean {
    return this.revoked.has(id);
  }
}

const b64 = (v: unknown): string => Buffer.from(JSON.stringify(v)).toString('base64url');
const JWT_HEADER = b64({ alg: 'HS256', typ: 'JWT' });
// Spent on unknown usernames so login timing does not reveal which users exist.
const DUMMY_HASH = 'scrypt$16384$8$1$AAAAAAAAAAAAAAAAAAAAAA==$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=';

export class AuthService {
  readonly config: AuthConfig;
  private readonly secret: string;
  private readonly revocations: RevocationList;
  private readonly users = new Map<string, UserDef>();
  private readonly keysByHash = new Map<string, { name: string; roles: string[] }>();
  private readonly keysByName = new Map<string, { name: string; roles: string[] }>();

  constructor(config: AuthConfig, secret: string, revocations = new RevocationList()) {
    this.config = config;
    this.secret = secret;
    this.revocations = revocations;
    for (const u of config.users) this.users.set(u.name, u);
    for (const k of config.apiKeys) {
      this.keysByHash.set(k.keyHash, k);
      this.keysByName.set(k.name, k);
    }
  }

  /** True when any user or API key is configured, i.e. somebody can authenticate. */
  get enabled(): boolean {
    return this.users.size > 0 || this.keysByHash.size > 0;
  }

  get hasUsers(): boolean {
    return this.users.size > 0;
  }

  get cookieName(): string {
    return this.config.cookieName;
  }

  async login(username: string, password: string): Promise<Principal | null> {
    const user = this.users.get(username);
    if (!user) {
      await verifyPassword(password, DUMMY_HASH);
      return null;
    }
    const ok = user.passwordHash ? await verifyPassword(password, user.passwordHash) : digestEqual(password, user.password ?? '');
    if (!ok) return null;
    return { id: `user:${user.name}`, name: user.name, kind: 'user', roles: [...user.roles], via: 'bearer' };
  }

  issueToken(principal: Principal, ttlMs = this.config.tokenTtlMs): IssuedToken {
    const now = Math.floor(Date.now() / 1000);
    const ttl = Math.max(1, Math.floor(Math.min(ttlMs, this.config.tokenTtlMs) / 1000));
    const payload: TokenPayload = { sub: principal.id, iat: now, exp: now + ttl, jti: randomId(9) };
    const body = `${JWT_HEADER}.${b64(payload)}`;
    return { token: `${body}.${this.sign(body)}`, expiresAt: payload.exp * 1000, id: payload.jti };
  }

  /** Verifies signature, expiry and revocation, then rebuilds the principal from current config. */
  verifyToken(token: string, via: Principal['via'] = 'bearer'): Principal | null {
    const parts = token.split('.');
    if (parts.length !== 3) return null;
    const [header, body, sig] = parts as [string, string, string];
    if (header !== JWT_HEADER) return null;
    const expected = Buffer.from(this.sign(`${header}.${body}`));
    const actual = Buffer.from(sig);
    if (expected.length !== actual.length || !timingSafeEqual(expected, actual)) return null;
    let payload: TokenPayload;
    try {
      payload = JSON.parse(Buffer.from(body, 'base64url').toString('utf8')) as TokenPayload;
    } catch {
      return null;
    }
    if (typeof payload.exp !== 'number' || payload.exp * 1000 <= Date.now()) return null;
    if (typeof payload.jti !== 'string' || this.revocations.has(payload.jti)) return null;
    const principal = this.lookupSubject(payload.sub);
    if (!principal) return null;
    return { ...principal, via, tokenId: payload.jti, tokenExpiresAt: payload.exp * 1000 };
  }

  revokeToken(principal: Principal): void {
    if (principal.tokenId && principal.tokenExpiresAt) this.revocations.revoke(principal.tokenId, principal.tokenExpiresAt);
  }

  verifyApiKey(key: string): Principal | null {
    const k = this.keysByHash.get(hashApiKey(key));
    if (!k) return null;
    return { id: `key:${k.name}`, name: k.name, kind: 'key', roles: [...k.roles], via: 'api-key' };
  }

  /**
   * Resolves the caller from (in order) an Authorization bearer credential
   * (Moxy token or API key), an X-API-Key header, or the session cookie.
   * Cookie sessions are ignored on cross-origin state-changing requests.
   */
  resolve(req: IncomingMessage): AuthResult {
    const authz = req.headers.authorization;
    if (authz && /^bearer\s+/i.test(authz)) {
      const credential = authz.replace(/^bearer\s+/i, '').trim();
      const principal = credential.split('.').length === 3 ? this.verifyToken(credential) : this.verifyApiKey(credential);
      return principal ? { principal } : { principal: null, error: 'Invalid or expired bearer credential' };
    }
    const apiKey = req.headers['x-api-key'];
    if (typeof apiKey === 'string' && apiKey) {
      const principal = this.verifyApiKey(apiKey);
      return principal ? { principal } : { principal: null, error: 'Invalid API key' };
    }
    const session = parseCookies(req.headers.cookie)[this.config.cookieName];
    if (session) {
      if (!sameOriginOrSafe(req)) return { principal: null, error: 'Cross-origin request with session cookie rejected' };
      const principal = this.verifyToken(session, 'cookie');
      return principal ? { principal } : { principal: null, error: 'Session expired' };
    }
    return { principal: null };
  }

  private lookupSubject(sub: string): Omit<Principal, 'via'> | null {
    if (typeof sub !== 'string') return null;
    if (sub.startsWith('user:')) {
      const user = this.users.get(sub.slice(5));
      return user ? { id: sub, name: user.name, kind: 'user', roles: [...user.roles] } : null;
    }
    if (sub.startsWith('key:')) {
      const key = this.keysByName.get(sub.slice(4));
      return key ? { id: sub, name: key.name, kind: 'key', roles: [...key.roles] } : null;
    }
    return null;
  }

  private sign(data: string): string {
    return createHmac('sha256', this.secret).update(data).digest('base64url');
  }
}

/** Safe methods always pass; unsafe ones must not carry a foreign Origin. */
export function sameOriginOrSafe(req: IncomingMessage): boolean {
  const method = (req.method ?? 'GET').toUpperCase();
  if (method === 'GET' || method === 'HEAD' || method === 'OPTIONS') return true;
  const origin = req.headers.origin;
  if (!origin) return true;
  try {
    return new URL(origin).host === req.headers.host;
  } catch {
    return false;
  }
}
