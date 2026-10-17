import assert from 'node:assert/strict';
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

const route = (id: string, middleware: string, extra = '') =>
  `<route id="${id}" path="/${id}/*" ${extra}><upstream url="UPSTREAM"/><middleware>${middleware}</middleware></route>`;

before(async () => {
  up = await startUpstream();
  const routes = [
    route('authz', '<auth roles="developer"/>'),
    route('authz-any', '<auth/>'),
    route('authz-optional', '<auth optional="true"/><set-header name="X-Who" value="{principal.id}"/>'),
    route('authz-keep', '<auth strip-credentials="false"/>'),
    route('ratelimit', '<rate-limit requests="2" window="1m" key="principal"/>'),
    route('quota', '<quota tokens="20" window="1h" key="principal"/>'),
    route('cors', '<cors origins="https://app.example.com, https://*.trusted.dev" credentials="true" expose="x-request-id"/>'),
    route(
      'headers',
      `<set-header name="X-Route" value="{route.id}:{request.id}"/>
       <set-header name="X-Keep" value="new" override="false"/>
       <remove-header name="x-secret, x-other"/>
       <set-header name="X-Served-By" value="moxy" phase="response"/>
       <remove-header name="x-upstream" phase="response"/>`,
    ),
    route('rewrite', '<rewrite-path from="^/old/(.*)$" to="/new/$1"/>'),
    route(
      'json',
      `<json-set path="temperature" value="0.2" type="number"/>
       <json-set path="metadata.source" value="moxy"/>
       <json-set path="user" value="default-user" mode="default"/>
       <max-tokens limit="100" default="50"/>`,
    ),
    route('models', '<model-filter allow="gpt-4*, claude-*" deny="*-preview" require="true"/>'),
    route('system', '<system-prompt mode="prepend">Be brief.</system-prompt>'),
    route('system-replace', '<system-prompt mode="replace">Only this.</system-prompt>'),
    route('system-append', '<system-prompt mode="append">After.</system-prompt>'),
    route('redact', '<redact patterns="email,credit-card,api-key"><pattern name="emp">EMP-\\d{4}</pattern></redact>'),
    route('redact-block', '<redact patterns="ssn" action="block"/>'),
    route('ipdeny', '<ip-filter deny="127.0.0.1"/>'),
    route('ipallow', '<ip-filter allow="127.0.0.0/8, ::1"/>'),
    route('bodylimit', '<body-limit max="20"/>'),
    route('cache', '<cache ttl="1m"/>'),
    route('chained', '<use chain="secure"/><set-header name="X-Chained" value="yes"/>'),
  ];
  const xml = `
    <moxy>
      ${AUTH_XML}
      <chains><chain name="secure"><auth roles="agent"/><rate-limit requests="100"/></chain></chains>
      <middleware><set-header name="X-Global" value="on"/></middleware>
      <routes>${routes.join('\n')}</routes>
    </moxy>`.replaceAll('UPSTREAM', '__UP__');
  gw = await startGateway(xml.replaceAll('__UP__', up.url));
});

after(async () => {
  await gw.close();
  await up.close();
});

const echo = async (res: Response) => (await res.json()) as any;

describe('auth middleware', () => {
  it('rejects anonymous callers with 401 and a challenge', async () => {
    const res = await fetch(`${gw.url}/authz/x`);
    assert.equal(res.status, 401);
    assert.equal(res.headers.get('www-authenticate'), 'Bearer realm="moxy"');
    assert.equal((await echo(res)).error.code, 'unauthorized');
  });

  it('rejects invalid credentials and wrong roles', async () => {
    assert.equal((await fetch(`${gw.url}/authz/x`, { headers: bearer('bogus-key-0000000000') })).status, 401);
    const wrongRole = await fetch(`${gw.url}/authz/x`, { headers: bearer(AGENT_KEY) });
    assert.equal(wrongRole.status, 403);
    assert.equal((await echo(wrongRole)).error.code, 'forbidden');
  });

  it('accepts API keys, tokens and admin, and strips Moxy credentials', async () => {
    const viaKey = await fetch(`${gw.url}/authz/x`, { headers: { 'x-api-key': DEV_KEY } });
    assert.equal(viaKey.status, 200);
    assert.equal((await echo(viaKey)).headers['x-api-key'], undefined);
    const { token } = await login(gw.url, 'dev', 'dev-pass');
    const viaToken = await fetch(`${gw.url}/authz/x`, { headers: bearer(token) });
    assert.equal(viaToken.status, 200);
    assert.equal((await echo(viaToken)).headers.authorization, undefined);
    assert.equal((await fetch(`${gw.url}/authz/x`, { headers: bearer(ADMIN_KEY) })).status, 200);
    assert.equal((await fetch(`${gw.url}/authz-any/x`, { headers: bearer(AGENT_KEY) })).status, 200);
  });

  it('accepts session cookies', async () => {
    const { cookie } = await login(gw.url, 'dev', 'dev-pass');
    const res = await fetch(`${gw.url}/authz/x`, { headers: { cookie } });
    assert.equal(res.status, 200);
    assert.equal((await echo(res)).headers.cookie, undefined);
  });

  it('can keep credentials when configured', async () => {
    const res = await fetch(`${gw.url}/authz-keep/x`, { headers: bearer(AGENT_KEY) });
    assert.equal((await echo(res)).headers.authorization, `Bearer ${AGENT_KEY}`);
  });

  it('lets anonymous callers through optional auth', async () => {
    const anon = await echo(await fetch(`${gw.url}/authz-optional/x`));
    assert.equal(anon.headers['x-who'], '');
    const known = await echo(await fetch(`${gw.url}/authz-optional/x`, { headers: bearer(AGENT_KEY) }));
    assert.equal(known.headers['x-who'], 'key:agent');
  });
});

describe('rate-limit and quota', () => {
  it('limits requests per principal with headers', async () => {
    const r1 = await fetch(`${gw.url}/ratelimit/x`, { headers: bearer(AGENT_KEY) });
    assert.equal(r1.status, 200);
    assert.equal(r1.headers.get('x-ratelimit-limit'), '2');
    assert.equal(r1.headers.get('x-ratelimit-remaining'), '1');
    assert.equal((await fetch(`${gw.url}/ratelimit/x`, { headers: bearer(AGENT_KEY) })).status, 200);
    const r3 = await fetch(`${gw.url}/ratelimit/x`, { headers: bearer(AGENT_KEY) });
    assert.equal(r3.status, 429);
    assert.ok(Number(r3.headers.get('retry-after')) >= 1);
    assert.equal((await echo(r3)).error.code, 'rate_limited');
    // A different principal has its own bucket.
    assert.equal((await fetch(`${gw.url}/ratelimit/x`, { headers: bearer(DEV_KEY) })).status, 200);
  });

  it('enforces token quotas from observed usage', async () => {
    const h = bearer(AGENT_KEY);
    const r1 = await fetch(`${gw.url}/quota/x`, { headers: h });
    assert.equal(r1.headers.get('x-moxy-quota-remaining'), '20');
    await r1.text();
    const r2 = await fetch(`${gw.url}/quota/x`, { headers: h }); // 15 used, 5 left
    assert.equal(r2.headers.get('x-moxy-quota-remaining'), '5');
    await r2.text();
    const r3 = await fetch(`${gw.url}/quota/x`, { headers: h }); // 30 used
    assert.equal(r3.status, 429);
    assert.equal((await echo(r3)).error.code, 'quota_exceeded');
  });
});

describe('cors', () => {
  it('answers preflights for allowed origins', async () => {
    const res = await fetch(`${gw.url}/cors/x`, {
      method: 'OPTIONS',
      headers: { origin: 'https://app.example.com', 'access-control-request-method': 'POST', 'access-control-request-headers': 'content-type' },
    });
    assert.equal(res.status, 204);
    assert.equal(res.headers.get('access-control-allow-origin'), 'https://app.example.com');
    assert.equal(res.headers.get('access-control-allow-credentials'), 'true');
    assert.equal(res.headers.get('access-control-allow-headers'), 'content-type');
    assert.equal(up.requests.filter((r) => r.method === 'OPTIONS').length, 0, 'preflight is not forwarded');
  });

  it('adds headers to actual responses and supports wildcards', async () => {
    const res = await fetch(`${gw.url}/cors/x`, { headers: { origin: 'https://a.trusted.dev' } });
    assert.equal(res.headers.get('access-control-allow-origin'), 'https://a.trusted.dev');
    assert.equal(res.headers.get('access-control-expose-headers'), 'x-request-id');
    assert.match(res.headers.get('vary') ?? '', /Origin/);
  });

  it('omits headers for other origins', async () => {
    const res = await fetch(`${gw.url}/cors/x`, { headers: { origin: 'https://evil.example' } });
    assert.equal(res.headers.get('access-control-allow-origin'), null);
  });
});

describe('header and path transforms', () => {
  it('sets and removes request and response headers', async () => {
    const res = await fetch(`${gw.url}/headers/x`, {
      headers: { 'x-secret': 's', 'x-other': 'o', 'x-keep': 'original', 'x-request-id': 'rid-1' },
    });
    const body = await echo(res);
    assert.equal(body.headers['x-route'], 'headers:rid-1');
    assert.equal(body.headers['x-keep'], 'original');
    assert.equal(body.headers['x-secret'], undefined);
    assert.equal(body.headers['x-other'], undefined);
    assert.equal(body.headers['x-global'], 'on', 'global middleware applies');
    assert.equal(res.headers.get('x-served-by'), 'moxy');
    assert.equal(res.headers.get('x-upstream'), null);
  });

  it('rewrites the upstream path', async () => {
    assert.equal((await echo(await fetch(`${gw.url}/rewrite/old/thing?q=1`))).url, '/new/thing?q=1');
    assert.equal((await echo(await fetch(`${gw.url}/rewrite/other`))).url, '/other');
  });

  it('applies chains', async () => {
    assert.equal((await fetch(`${gw.url}/chained/x`)).status, 401);
    const body = await echo(await fetch(`${gw.url}/chained/x`, { headers: bearer(AGENT_KEY) }));
    assert.equal(body.headers['x-chained'], 'yes');
  });
});

describe('JSON body transforms', () => {
  it('sets fields and caps max tokens', async () => {
    const body = (await echo(await postJson(`${gw.url}/json/x`, { model: 'm', max_tokens: 5000, user: 'me' }))).body;
    assert.equal(body.temperature, 0.2);
    assert.deepEqual(body.metadata, { source: 'moxy' });
    assert.equal(body.user, 'me');
    assert.equal(body.max_tokens, 100);
  });

  it('defaults max tokens and missing fields', async () => {
    const body = (await echo(await postJson(`${gw.url}/json/x`, { model: 'm' }))).body;
    assert.equal(body.max_tokens, 50);
    assert.equal(body.user, 'default-user');
  });

  it('leaves non-JSON bodies alone', async () => {
    const res = await fetch(`${gw.url}/json/x`, { method: 'POST', headers: { 'content-type': 'text/plain' }, body: 'hello' });
    assert.equal((await echo(res)).body, 'hello');
  });

  it('filters models', async () => {
    assert.equal((await postJson(`${gw.url}/models/x`, { model: 'gpt-4o' })).status, 200);
    assert.equal((await postJson(`${gw.url}/models/x`, { model: 'claude-3' })).status, 200);
    const denied = await postJson(`${gw.url}/models/x`, { model: 'gpt-4-preview' });
    assert.equal(denied.status, 403);
    assert.equal((await echo(denied)).error.code, 'model_not_allowed');
    assert.equal((await postJson(`${gw.url}/models/x`, { model: 'llama' })).status, 403);
    assert.equal((await postJson(`${gw.url}/models/x`, { prompt: 'x' })).status, 400);
  });

  it('injects system prompts in OpenAI format', async () => {
    const messages = [
      { role: 'system', content: 'Original.' },
      { role: 'user', content: 'hi' },
    ];
    const pre = (await echo(await postJson(`${gw.url}/system/x`, { messages }))).body.messages;
    assert.deepEqual(pre.map((m: any) => m.content), ['Be brief.', 'Original.', 'hi']);
    const rep = (await echo(await postJson(`${gw.url}/system-replace/x`, { messages }))).body.messages;
    assert.deepEqual(rep.map((m: any) => m.content), ['Only this.', 'hi']);
    const app = (await echo(await postJson(`${gw.url}/system-append/x`, { messages }))).body.messages;
    assert.deepEqual(app.map((m: any) => m.content), ['Original.', 'After.', 'hi']);
  });

  it('injects system prompts in Anthropic format', async () => {
    const withSystem = (await echo(await postJson(`${gw.url}/system/x`, { system: 'Original.', messages: [] }))).body;
    assert.equal(withSystem.system, 'Be brief.\n\nOriginal.');
    const viaHeader = (await echo(await postJson(`${gw.url}/system/x`, { messages: [] }, { 'anthropic-version': '2023-06-01' }))).body;
    assert.equal(viaHeader.system, 'Be brief.');
    assert.deepEqual(viaHeader.messages, []);
  });

  it('redacts sensitive data', async () => {
    const res = await postJson(`${gw.url}/redact/x`, {
      model: 'keep@model.com',
      messages: [{ role: 'user', content: 'mail bob@example.com card 4111 1111 1111 1111 not 1234 5678 9012 3456 key sk-abcdefghijklmnopqrstuv EMP-1234' }],
    });
    assert.equal(res.headers.get('x-moxy-redactions'), '4');
    const body = (await echo(res)).body;
    assert.equal(body.model, 'keep@model.com');
    assert.equal(
      body.messages[0].content,
      'mail [REDACTED:email] card [REDACTED:credit-card] not 1234 5678 9012 3456 key [REDACTED:api-key] [REDACTED:emp]',
    );
  });

  it('blocks sensitive data when configured', async () => {
    const res = await postJson(`${gw.url}/redact-block/x`, { messages: [{ role: 'user', content: 'ssn 123-45-6789' }] });
    assert.equal(res.status, 400);
    assert.equal((await echo(res)).error.code, 'sensitive_content');
    assert.equal((await postJson(`${gw.url}/redact-block/x`, { messages: [] })).status, 200);
  });
});

describe('access limits', () => {
  it('filters by IP', async () => {
    assert.equal((await fetch(`${gw.url}/ipdeny/x`)).status, 403);
    assert.equal((await fetch(`${gw.url}/ipallow/x`)).status, 200);
  });

  it('limits body size per route', async () => {
    assert.equal((await postJson(`${gw.url}/bodylimit/x`, { a: 1 })).status, 200);
    assert.equal((await postJson(`${gw.url}/bodylimit/x`, { a: 'x'.repeat(50) })).status, 413);
  });
});

describe('cache middleware', () => {
  it('serves repeated identical requests from cache', async () => {
    const before = up.requests.length;
    const r1 = await postJson(`${gw.url}/cache/chat`, { model: 'm', prompt: 'same' });
    assert.equal(r1.headers.get('x-moxy-cache'), 'MISS');
    const b1 = await r1.text();
    const r2 = await postJson(`${gw.url}/cache/chat`, { model: 'm', prompt: 'same' });
    assert.equal(r2.headers.get('x-moxy-cache'), 'HIT');
    assert.equal(await r2.text(), b1);
    assert.equal(up.requests.length - before, 1);
    const r3 = await postJson(`${gw.url}/cache/chat`, { model: 'm', prompt: 'different' });
    assert.equal(r3.headers.get('x-moxy-cache'), 'MISS');
    await r3.text();
    assert.equal(up.requests.length - before, 2);
  });

  it('does not cache streaming requests or no-cache requests', async () => {
    const before = up.requests.length;
    for (let i = 0; i < 2; i++) await (await postJson(`${gw.url}/cache/chat`, { stream: true, prompt: 's' })).text();
    for (let i = 0; i < 2; i++) await (await postJson(`${gw.url}/cache/chat`, { prompt: 'nc' }, { 'cache-control': 'no-cache' })).text();
    assert.equal(up.requests.length - before, 4);
  });

  it('separates cache entries per principal', async () => {
    const before = up.requests.length;
    await (await postJson(`${gw.url}/cache/chat`, { prompt: 'pp' }, bearer(AGENT_KEY))).text();
    const other = await postJson(`${gw.url}/cache/chat`, { prompt: 'pp' }, bearer(DEV_KEY));
    assert.equal(other.headers.get('x-moxy-cache'), 'MISS');
    await other.text();
    assert.equal(up.requests.length - before, 2);
  });
});
