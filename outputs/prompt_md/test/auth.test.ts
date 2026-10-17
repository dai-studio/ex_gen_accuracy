import assert from 'node:assert/strict';
import type { IncomingMessage } from 'node:http';
import { describe, it } from 'node:test';
import { AuthService, hasRole, hashApiKey, hashPassword, RevocationList, verifyPassword, type Principal } from '../src/auth.ts';
import type { AuthConfig } from '../src/config.ts';

async function makeService(revocations = new RevocationList()) {
  const config: AuthConfig = {
    secret: 'unit-test-secret-0123456789',
    tokenTtlMs: 3_600_000,
    cookieName: 'moxy_session',
    users: [
      { name: 'alice', roles: ['developer'], passwordHash: await hashPassword('wonderland') },
      { name: 'bob', roles: ['admin'], password: 'builder' },
    ],
    apiKeys: [{ name: 'bot', roles: ['agent'], keyHash: hashApiKey('bot-key-0123456789') }],
  };
  return new AuthService(config, config.secret, revocations);
}

const fakeReq = (headers: Record<string, string>, method = 'GET') => ({ headers, method }) as unknown as IncomingMessage;

describe('passwords', () => {
  it('hashes and verifies with scrypt', async () => {
    const hash = await hashPassword('s3cret');
    assert.match(hash, /^scrypt\$16384\$8\$1\$/);
    assert.equal(await verifyPassword('s3cret', hash), true);
    assert.equal(await verifyPassword('wrong', hash), false);
    assert.equal(await verifyPassword('s3cret', 'garbage'), false);
  });
});

describe('AuthService', () => {
  it('logs users in with hashed or plain passwords', async () => {
    const auth = await makeService();
    assert.equal((await auth.login('alice', 'wonderland'))?.id, 'user:alice');
    assert.equal((await auth.login('bob', 'builder'))?.id, 'user:bob');
    assert.equal(await auth.login('alice', 'nope'), null);
    assert.equal(await auth.login('nobody', 'x'), null);
  });

  it('issues and verifies tokens', async () => {
    const auth = await makeService();
    const alice = (await auth.login('alice', 'wonderland'))!;
    const { token, expiresAt } = auth.issueToken(alice);
    assert.ok(expiresAt > Date.now());
    const p = auth.verifyToken(token)!;
    assert.equal(p.id, 'user:alice');
    assert.deepEqual(p.roles, ['developer']);
    assert.equal(p.via, 'bearer');
  });

  it('rejects tampered, foreign and expired tokens', async () => {
    const auth = await makeService();
    const alice = (await auth.login('alice', 'wonderland'))!;
    const { token } = auth.issueToken(alice);
    const [h, b, s] = token.split('.');
    const forged = Buffer.from(JSON.stringify({ ...JSON.parse(Buffer.from(b!, 'base64url').toString()), sub: 'user:bob' })).toString('base64url');
    assert.equal(auth.verifyToken(`${h}.${forged}.${s}`), null);
    assert.equal(auth.verifyToken(`${h}.${b}.${s}x`), null);
    const other = new AuthService({ ...auth.config }, 'a-different-secret-xyz', new RevocationList());
    assert.equal(other.verifyToken(token), null);
    const expired = auth.issueToken(alice, 1);
    await new Promise((r) => setTimeout(r, 1100));
    assert.equal(auth.verifyToken(expired.token), null);
  });

  it('refreshes roles from current config and drops removed users', async () => {
    const auth = await makeService();
    const alice = (await auth.login('alice', 'wonderland'))!;
    const { token } = auth.issueToken(alice);
    const changed = new AuthService(
      { ...auth.config, users: [{ name: 'alice', roles: ['admin'], password: 'x' }] },
      auth.config.secret,
      new RevocationList(),
    );
    assert.deepEqual(changed.verifyToken(token)!.roles, ['admin']);
    const removed = new AuthService({ ...auth.config, users: [] }, auth.config.secret, new RevocationList());
    assert.equal(removed.verifyToken(token), null);
  });

  it('revokes tokens', async () => {
    const revocations = new RevocationList();
    const auth = await makeService(revocations);
    const { token } = auth.issueToken((await auth.login('alice', 'wonderland'))!);
    const p = auth.verifyToken(token)!;
    auth.revokeToken(p);
    assert.equal(auth.verifyToken(token), null);
  });

  it('resolves bearer tokens, API keys and cookies', async () => {
    const auth = await makeService();
    const { token } = auth.issueToken((await auth.login('alice', 'wonderland'))!);
    assert.equal(auth.resolve(fakeReq({ authorization: `Bearer ${token}` })).principal?.id, 'user:alice');
    assert.equal(auth.resolve(fakeReq({ authorization: 'Bearer bot-key-0123456789' })).principal?.id, 'key:bot');
    assert.equal(auth.resolve(fakeReq({ 'x-api-key': 'bot-key-0123456789' })).principal?.via, 'api-key');
    assert.equal(auth.resolve(fakeReq({ cookie: `other=1; moxy_session=${token}` })).principal?.via, 'cookie');
    const bad = auth.resolve(fakeReq({ authorization: 'Bearer nope' }));
    assert.equal(bad.principal, null);
    assert.ok(bad.error);
    assert.deepEqual(auth.resolve(fakeReq({})), { principal: null });
  });

  it('ignores session cookies on cross-origin unsafe requests', async () => {
    const auth = await makeService();
    const { token } = auth.issueToken((await auth.login('alice', 'wonderland'))!);
    const cookie = `moxy_session=${token}`;
    assert.equal(auth.resolve(fakeReq({ cookie, origin: 'https://evil.example', host: 'moxy.local' }, 'POST')).principal, null);
    assert.ok(auth.resolve(fakeReq({ cookie, origin: 'http://moxy.local', host: 'moxy.local' }, 'POST')).principal);
    assert.ok(auth.resolve(fakeReq({ cookie, origin: 'https://evil.example', host: 'moxy.local' }, 'GET')).principal);
  });

  it('mints tokens for API key principals', async () => {
    const auth = await makeService();
    const bot = auth.verifyApiKey('bot-key-0123456789')!;
    const { token } = auth.issueToken(bot, 60_000);
    assert.equal(auth.verifyToken(token)?.id, 'key:bot');
  });
});

describe('hasRole', () => {
  const p = (roles: string[]): Principal => ({ id: 'user:x', name: 'x', kind: 'user', roles, via: 'bearer' });
  it('checks roles with admin override', () => {
    assert.ok(hasRole(p([]), []));
    assert.ok(hasRole(p(['developer']), ['developer', 'agent']));
    assert.ok(!hasRole(p(['agent']), ['developer']));
    assert.ok(hasRole(p(['admin']), ['developer']));
  });
});
