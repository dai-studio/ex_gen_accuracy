import assert from 'node:assert/strict';
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { after, before, describe, it } from 'node:test';
import {
  ADMIN_KEY,
  AGENT_KEY,
  AUTH_XML,
  bearer,
  DEV_KEY,
  login,
  postJson,
  startGateway,
  startUpstream,
  type MockUpstream,
  type TestGateway,
} from './helpers.ts';

let up: MockUpstream;
let gw: TestGateway;
const TMP = join(import.meta.dirname, '.tmp-builtins');
const CONFIG_PATH = join(TMP, 'moxy.xml');

function configXml(upstreamUrl: string, extraRoute = ''): string {
  return `
    <moxy>
      ${AUTH_XML}
      <builtins title="Test Gate"/>
      <routes>
        <route id="agents" path="/agents/*">
          <description>Agent models</description>
          <upstream url="${upstreamUrl}/v1">
            <header name="Authorization" value="Bearer provider-secret"/>
          </upstream>
          <models chat-path="/chat/completions" embeddings-path="/embeddings">
            <model id="agent-model" upstream-model="real-agent-model" description="For agents"/>
            <model id="embedder"/>
          </models>
          <middleware><auth roles="agent"/></middleware>
        </route>
        <route id="devs" path="/devs/*">
          <upstream url="${upstreamUrl}/dev"/>
          <models><model id="dev-model"/></models>
          <middleware><auth roles="developer"/></middleware>
        </route>
        <route id="public" path="/public/*">
          <upstream url="${upstreamUrl}/public"/>
          <models><model id="public-model"/></models>
        </route>
        <route id="secret" path="/secret/*" hidden="true">
          <upstream url="${upstreamUrl}"/>
          <models><model id="hidden-model"/></models>
        </route>
        ${extraRoute}
      </routes>
    </moxy>`;
}

before(async () => {
  up = await startUpstream();
  mkdirSync(TMP, { recursive: true });
  writeFileSync(CONFIG_PATH, configXml(up.url));
  gw = await startGateway(configXml(up.url), {}, CONFIG_PATH);
});

after(async () => {
  await gw.close();
  await up.close();
  rmSync(TMP, { recursive: true, force: true });
});

const json = async (res: Response) => (await res.json()) as any;

describe('health and index', () => {
  it('serves /healthz', async () => {
    const body = await json(await fetch(`${gw.url}/healthz`));
    assert.equal(body.status, 'ok');
    assert.ok(body.version);
  });

  it('serves a landing page at / with security headers', async () => {
    const res = await fetch(`${gw.url}/`);
    assert.equal(res.status, 200);
    assert.match(res.headers.get('content-type')!, /text\/html/);
    assert.match(res.headers.get('content-security-policy')!, /script-src 'nonce-/);
    const html = await res.text();
    assert.match(html, /Test Gate/);
    assert.match(html, /href="\/chat"/);
  });
});

describe('/auth', () => {
  it('logs in with JSON and sets a hardened session cookie', async () => {
    const res = await postJson(`${gw.url}/auth/login`, { username: 'dev', password: 'dev-pass' });
    assert.equal(res.status, 200);
    const body = await json(res);
    assert.equal(body.token_type, 'Bearer');
    assert.deepEqual(body.user, { name: 'dev', roles: ['developer'] });
    const cookie = res.headers.get('set-cookie')!;
    assert.match(cookie, /^moxy_session=/);
    assert.match(cookie, /HttpOnly/);
    assert.match(cookie, /SameSite=Strict/);
    assert.match(cookie, /Path=\//);
  });

  it('rejects bad credentials', async () => {
    const res = await postJson(`${gw.url}/auth/login`, { username: 'dev', password: 'nope' });
    assert.equal(res.status, 401);
    assert.equal((await json(res)).error.code, 'invalid_credentials');
    assert.equal((await postJson(`${gw.url}/auth/login`, { username: 'dev' })).status, 400);
  });

  it('supports HTML form login with safe redirects', async () => {
    const ok = await fetch(`${gw.url}/auth/login`, {
      method: 'POST',
      redirect: 'manual',
      headers: { 'content-type': 'application/x-www-form-urlencoded' },
      body: 'username=dev&password=dev-pass&next=%2Fchat',
    });
    assert.equal(ok.status, 303);
    assert.equal(ok.headers.get('location'), '/chat');
    assert.ok(ok.headers.get('set-cookie'));
    const evil = await fetch(`${gw.url}/auth/login`, {
      method: 'POST',
      redirect: 'manual',
      headers: { 'content-type': 'application/x-www-form-urlencoded' },
      body: 'username=dev&password=dev-pass&next=%2F%2Fevil.com',
    });
    assert.equal(evil.headers.get('location'), '/chat');
    const bad = await fetch(`${gw.url}/auth/login`, {
      method: 'POST',
      redirect: 'manual',
      headers: { 'content-type': 'application/x-www-form-urlencoded' },
      body: 'username=dev&password=wrong',
    });
    assert.equal(bad.status, 303);
    assert.match(bad.headers.get('location')!, /error=invalid_credentials/);
  });

  it('serves the login page', async () => {
    const res = await fetch(`${gw.url}/auth/login?next=/admin&error=invalid_credentials`);
    const html = await res.text();
    assert.match(html, /name="next" value="\/admin"/);
    assert.match(html, /Incorrect username or password/);
  });

  it('reports the current principal', async () => {
    assert.equal((await fetch(`${gw.url}/auth/me`)).status, 401);
    const { token } = await login(gw.url, 'admin', 'admin-pass');
    const me = await json(await fetch(`${gw.url}/auth/me`, { headers: bearer(token) }));
    assert.equal(me.id, 'user:admin');
    assert.deepEqual(me.roles, ['admin']);
    const key = await json(await fetch(`${gw.url}/auth/me`, { headers: { 'x-api-key': AGENT_KEY } }));
    assert.equal(key.kind, 'key');
  });

  it('revokes tokens on logout', async () => {
    const { token, cookie } = await login(gw.url, 'dev', 'dev-pass');
    const res = await postJson(`${gw.url}/auth/logout`, {}, { cookie });
    assert.equal(res.status, 200);
    assert.match(res.headers.get('set-cookie')!, /Max-Age=0/);
    assert.equal((await fetch(`${gw.url}/auth/me`, { headers: { cookie } })).status, 401);
    // The same JWT presented as a bearer token is revoked too.
    assert.equal((await fetch(`${gw.url}/auth/me`, { headers: bearer(token) })).status, 401);
  });

  it('mints short-lived tokens from API keys', async () => {
    const res = await postJson(`${gw.url}/auth/token`, { ttl: '5m' }, bearer(AGENT_KEY));
    const body = await json(res);
    assert.equal(res.status, 200);
    const ttl = Date.parse(body.expires_at) - Date.now();
    assert.ok(ttl > 4 * 60_000 && ttl <= 5 * 60_000);
    const me = await json(await fetch(`${gw.url}/auth/me`, { headers: bearer(body.token) }));
    assert.equal(me.id, 'key:agent');
    assert.equal((await postJson(`${gw.url}/auth/token`, {})).status, 401);
    assert.equal((await postJson(`${gw.url}/auth/token`, { ttl: 'forever' }, bearer(AGENT_KEY))).status, 400);
  });

  it('locks out an IP after repeated failures but not after successes', async () => {
    const g = await startGateway(`<moxy>${AUTH_XML}</moxy>`);
    try {
      for (let i = 0; i < 12; i++) await login(g.url, 'dev', 'dev-pass');
      for (let i = 0; i < 10; i++) {
        assert.equal((await postJson(`${g.url}/auth/login`, { username: 'dev', password: 'wrong' })).status, 401);
      }
      const locked = await postJson(`${g.url}/auth/login`, { username: 'dev', password: 'dev-pass' });
      assert.equal(locked.status, 429);
      assert.equal((await json(locked)).error.code, 'too_many_attempts');
    } finally {
      await g.close();
    }
  });

  it('rejects wrong methods', async () => {
    const res = await fetch(`${gw.url}/auth/logout`);
    assert.equal(res.status, 405);
    assert.equal(res.headers.get('allow'), 'POST');
  });
});

describe('/admin', () => {
  it('requires the admin role', async () => {
    assert.equal((await fetch(`${gw.url}/admin/api/status`)).status, 401);
    assert.equal((await fetch(`${gw.url}/admin/api/status`, { headers: bearer(DEV_KEY) })).status, 403);
    assert.equal((await fetch(`${gw.url}/admin/api/status`, { headers: bearer(ADMIN_KEY) })).status, 200);
  });

  it('redirects anonymous browsers to the login page', async () => {
    const res = await fetch(`${gw.url}/admin`, { redirect: 'manual' });
    assert.equal(res.status, 302);
    assert.equal(res.headers.get('location'), '/auth/login?next=%2Fadmin');
  });

  it('serves the dashboard to admins', async () => {
    const { cookie } = await login(gw.url, 'admin', 'admin-pass');
    const res = await fetch(`${gw.url}/admin`, { headers: { cookie } });
    assert.equal(res.status, 200);
    assert.match(await res.text(), /Signed in as admin/);
  });

  it('reports status, routes and stats without leaking header values', async () => {
    const h = bearer(ADMIN_KEY);
    const status = await json(await fetch(`${gw.url}/admin/api/status`, { headers: h }));
    assert.equal(status.routes, 4);
    assert.equal(status.auth.secret, 'configured');
    const routes = await json(await fetch(`${gw.url}/admin/api/routes`, { headers: h }));
    const agents = routes.routes.find((r: any) => r.id === 'agents');
    assert.deepEqual(agents.middleware, ['auth']);
    assert.deepEqual(agents.upstreams[0].injected_headers, ['authorization']);
    assert.ok(!JSON.stringify(routes).includes('provider-secret'));
    assert.equal(agents.upstreams[0].health.healthy, true);
    await (await fetch(`${gw.url}/public/x`)).text();
    const stats = await json(await fetch(`${gw.url}/admin/api/stats`, { headers: h }));
    assert.ok(stats.routes.public.requests >= 1);
    const reset = await fetch(`${gw.url}/admin/api/stats/reset`, { method: 'POST', headers: h });
    assert.equal(reset.status, 200);
    const after = await json(await fetch(`${gw.url}/admin/api/stats`, { headers: h }));
    assert.equal(after.total.requests, 0);
  });

  it('rejects cross-origin cookie requests', async () => {
    const { cookie } = await login(gw.url, 'admin', 'admin-pass');
    const res = await fetch(`${gw.url}/admin/api/stats/reset`, { method: 'POST', headers: { cookie, origin: 'https://evil.example' } });
    assert.equal(res.status, 401);
  });
});

describe('/api/v1', () => {
  it('lists the models the caller may use', async () => {
    const ids = async (headers: Record<string, string> = {}) =>
      (await json(await fetch(`${gw.url}/api/v1/models`, { headers }))).data.map((m: any) => m.id).sort();
    assert.deepEqual(await ids(), ['public-model']);
    assert.deepEqual(await ids(bearer(AGENT_KEY)), ['agent-model', 'embedder', 'public-model']);
    assert.deepEqual(await ids(bearer(DEV_KEY)), ['dev-model', 'public-model']);
    assert.deepEqual(await ids(bearer(ADMIN_KEY)), ['agent-model', 'dev-model', 'embedder', 'public-model']);
  });

  it('describes a single model', async () => {
    const res = await fetch(`${gw.url}/api/v1/models/agent-model`, { headers: bearer(AGENT_KEY) });
    const body = await json(res);
    assert.equal(body.owned_by, 'agents');
    assert.equal(body.description, 'For agents');
    assert.equal((await fetch(`${gw.url}/api/v1/models/agent-model`)).status, 404);
  });

  it('dispatches chat completions by model through the route middleware', async () => {
    const res = await postJson(`${gw.url}/api/v1/chat/completions`, { model: 'agent-model', messages: [] }, bearer(AGENT_KEY));
    assert.equal(res.status, 200);
    const echo = await json(res);
    assert.equal(echo.url, '/v1/chat/completions');
    assert.equal(echo.body.model, 'real-agent-model');
    assert.equal(echo.headers.authorization, 'Bearer provider-secret');
    const snap = gw.gateway.stats.snapshot();
    assert.equal(snap.models['agent-model']!.totalTokens, 15);
    assert.equal(snap.principals['key:agent']!.requests >= 1, true);
  });

  it('enforces route auth on dispatched requests', async () => {
    assert.equal((await postJson(`${gw.url}/api/v1/chat/completions`, { model: 'agent-model' })).status, 401);
    assert.equal((await postJson(`${gw.url}/api/v1/chat/completions`, { model: 'agent-model' }, bearer(DEV_KEY))).status, 403);
    assert.equal((await postJson(`${gw.url}/api/v1/chat/completions`, { model: 'public-model' })).status, 200);
  });

  it('works with a browser session cookie', async () => {
    const { cookie } = await login(gw.url, 'dev', 'dev-pass');
    const res = await postJson(`${gw.url}/api/v1/chat/completions`, { model: 'dev-model' }, { cookie });
    assert.equal(res.status, 200);
    const echo = await json(res);
    assert.equal(echo.url, '/dev/chat/completions');
    assert.equal(echo.headers.cookie, undefined);
  });

  it('dispatches embeddings', async () => {
    const echo = await json(await postJson(`${gw.url}/api/v1/embeddings`, { model: 'embedder', input: 'x' }, bearer(AGENT_KEY)));
    assert.equal(echo.url, '/v1/embeddings');
  });

  it('validates requests', async () => {
    const unknown = await postJson(`${gw.url}/api/v1/chat/completions`, { model: 'nope' });
    assert.equal(unknown.status, 404);
    assert.equal((await json(unknown)).error.code, 'model_not_found');
    assert.equal((await postJson(`${gw.url}/api/v1/chat/completions`, { messages: [] })).status, 400);
    assert.equal((await postJson(`${gw.url}/api/v1/chat/completions`, { model: 'hidden-model' })).status, 404);
    const notJson = await fetch(`${gw.url}/api/v1/chat/completions`, { method: 'POST', body: 'hello' });
    assert.equal(notJson.status, 400);
    assert.equal((await fetch(`${gw.url}/api/v1/chat/completions`)).status, 405);
    assert.equal((await fetch(`${gw.url}/api/v1/unknown`)).status, 404);
  });
});

describe('/chat', () => {
  it('requires login when users exist', async () => {
    const res = await fetch(`${gw.url}/chat`, { redirect: 'manual' });
    assert.equal(res.status, 302);
    assert.equal(res.headers.get('location'), '/auth/login?next=%2Fchat');
  });

  it('serves the chat UI to signed-in users', async () => {
    const { cookie } = await login(gw.url, 'dev', 'dev-pass');
    const res = await fetch(`${gw.url}/chat`, { headers: { cookie } });
    assert.equal(res.status, 200);
    const html = await res.text();
    assert.match(html, /data-user="dev"/);
    assert.match(html, /data-can-logout="true"/);
    assert.equal((await fetch(`${gw.url}/chat/other`, { headers: { cookie } })).status, 404);
  });

  it('can be opened without login when chat-auth is off', async () => {
    const open = await startGateway(`<moxy>${AUTH_XML}<builtins chat-auth="false"/></moxy>`);
    try {
      assert.equal((await fetch(`${open.url}/chat`)).status, 200);
    } finally {
      await open.close();
    }
  });
});

describe('discovery', () => {
  it('describes endpoints and visible routes', async () => {
    const d = await json(await fetch(`${gw.url}/.well-known/moxy.json`));
    assert.equal(d.name, 'moxy');
    assert.equal(d.title, 'Test Gate');
    assert.equal(d.endpoints.api.openai_base_url, '/api/v1');
    assert.equal(d.endpoints.login, '/auth/login');
    assert.equal(d.authentication.enabled, true);
    assert.equal(d.caller, null);
    const ids = d.routes.map((r: any) => r.id);
    assert.deepEqual(ids, ['agents', 'devs', 'public']);
    const agents = d.routes.find((r: any) => r.id === 'agents');
    assert.deepEqual(agents.auth, { required: true, roles: ['agent'] });
    assert.equal(agents.accessible, false);
    assert.deepEqual(agents.models, ['agent-model', 'embedder']);
    assert.equal(agents.description, 'Agent models');
    assert.ok(!JSON.stringify(d).includes(up.url), 'upstream URLs are not disclosed');
  });

  it('reflects the caller', async () => {
    const d = await json(await fetch(`${gw.url}/.well-known/moxy.json`, { headers: bearer(AGENT_KEY) }));
    assert.equal(d.caller.id, 'key:agent');
    assert.equal(d.routes.find((r: any) => r.id === 'agents').accessible, true);
  });
});

describe('disabled built-ins', () => {
  it('returns 404 and frees the paths for routes', async () => {
    const upstream = await startUpstream();
    const g = await startGateway(`
      <moxy>
        <builtins admin="false" auth="false" chat="false" api="false" discovery="false"/>
        <routes><route path="/api/*"><upstream url="${upstream.url}"/></route></routes>
      </moxy>`);
    try {
      assert.equal((await fetch(`${g.url}/admin`)).status, 404);
      assert.equal((await fetch(`${g.url}/chat`)).status, 404);
      assert.equal((await fetch(`${g.url}/.well-known/moxy.json`)).status, 404);
      assert.equal((await fetch(`${g.url}/`)).status, 404);
      assert.equal((await fetch(`${g.url}/healthz`)).status, 200);
      const echo = await json(await fetch(`${g.url}/api/v1/models`));
      assert.equal(echo.url, '/v1/models');
    } finally {
      await g.close();
      await upstream.close();
    }
  });
});

describe('reload', () => {
  it('reloads a valid config and keeps the old one on errors', async () => {
    const h = bearer(ADMIN_KEY);
    writeFileSync(CONFIG_PATH, configXml(up.url, `<route id="added" path="/added/*"><upstream url="${up.url}/added"/></route>`));
    const ok = await fetch(`${gw.url}/admin/api/reload`, { method: 'POST', headers: h });
    const okBody = await json(ok);
    assert.equal(ok.status, 200);
    assert.equal(okBody.routes, 5);
    assert.equal((await json(await fetch(`${gw.url}/added/x`))).url, '/added/x');

    writeFileSync(CONFIG_PATH, '<moxy><routes><route path="broken"/></routes></moxy>');
    const bad = await fetch(`${gw.url}/admin/api/reload`, { method: 'POST', headers: h });
    assert.equal(bad.status, 422);
    assert.match((await json(bad)).error, /must start with/);
    assert.equal((await fetch(`${gw.url}/added/x`)).status, 200, 'previous config still active');
  });

  it('keeps sessions valid across reloads', async () => {
    writeFileSync(CONFIG_PATH, configXml(up.url));
    const { token } = await login(gw.url, 'dev', 'dev-pass');
    assert.equal(gw.gateway.reload().ok, true);
    assert.equal((await fetch(`${gw.url}/auth/me`, { headers: bearer(token) })).status, 200);
  });
});
