import assert from 'node:assert/strict';
import http from 'node:http';
import { after, before, describe, it } from 'node:test';
import { AGENT_KEY, AUTH_XML, bearer, postJson, rawRequest, startGateway, startUpstream, type MockUpstream, type TestGateway } from './helpers.ts';

describe('proxying', () => {
  let up: MockUpstream;
  let up2: MockUpstream;
  let gw: TestGateway;

  before(async () => {
    up = await startUpstream();
    up2 = await startUpstream();
    gw = await startGateway(`
      <moxy>
        ${AUTH_XML}
        <server max-body="1kb"/>
        <upstreams>
          <upstream name="main" url="${up.url}/v1">
            <header name="Authorization" value="Bearer provider-secret"/>
          </upstream>
        </upstreams>
        <routes>
          <route id="ai" path="/ai/*">
            <upstream ref="main"/>
            <models><model id="fast" upstream-model="real-model"/></models>
          </route>
          <route id="exact" path="/exact" methods="POST">
            <upstream url="${up.url}/fixed/endpoint"/>
          </route>
          <route id="rr" path="/rr/*" strategy="round-robin" retries="0">
            <upstream url="${up.url}"/>
            <upstream url="${up2.url}"/>
          </route>
          <route id="preserve" path="/preserve/*" preserve-host="true">
            <upstream url="${up.url}"/>
          </route>
        </routes>
      </moxy>`);
  });
  after(async () => {
    await gw.close();
    await up.close();
    await up2.close();
  });

  it('forwards prefix routes with path, query, body and rewritten headers', async () => {
    const res = await rawRequest(gw.url, '/ai/chat/completions?x=1&y=2', {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: 'Bearer caller-provided',
        cookie: 'moxy_session=abc; theme=dark',
        'x-request-id': 'req-123',
        connection: 'keep-alive, x-hop',
        'x-hop': 'drop me',
        'x-custom': 'kept',
      },
      body: JSON.stringify({ model: 'gpt', messages: [] }),
    });
    assert.equal(res.status, 200);
    assert.equal(res.headers['x-request-id'], 'req-123');
    assert.equal(res.headers['x-upstream-request-id'], 'upstream-id');
    assert.equal(res.headers['x-upstream'], 'yes');
    const echo = JSON.parse(res.body);
    assert.equal(echo.method, 'POST');
    assert.equal(echo.url, '/v1/chat/completions?x=1&y=2');
    assert.deepEqual(echo.body, { model: 'gpt', messages: [] });
    const h = echo.headers;
    assert.equal(h.host, `127.0.0.1:${up.port}`);
    assert.equal(h.authorization, 'Bearer provider-secret', 'upstream header injection overrides caller');
    assert.equal(h.cookie, 'theme=dark', 'moxy session cookie is stripped');
    assert.equal(h['x-forwarded-for'], '127.0.0.1');
    assert.equal(h['x-forwarded-proto'], 'http');
    assert.equal(h['x-request-id'], 'req-123');
    assert.equal(h['accept-encoding'], 'identity');
    assert.equal(h['x-custom'], 'kept');
    assert.equal(h['x-hop'], undefined, 'headers named in Connection are dropped');
  });

  it('forwards exact routes to the upstream URL as configured', async () => {
    const res = await postJson(`${gw.url}/exact?q=1`, { a: 1 });
    const echo = (await res.json()) as any;
    assert.equal(echo.url, '/fixed/endpoint?q=1');
  });

  it('rewrites aliased model names', async () => {
    const res = await postJson(`${gw.url}/ai/chat/completions`, { model: 'fast' });
    const echo = (await res.json()) as any;
    assert.equal(echo.body.model, 'real-model');
  });

  it('preserves the Host header when asked', async () => {
    const res = await fetch(`${gw.url}/preserve/x`);
    const echo = (await res.json()) as any;
    assert.equal(echo.headers.host, new URL(gw.url).host);
  });

  it('returns 404, 405 and 400 for unroutable requests', async () => {
    const notFound = await fetch(`${gw.url}/nowhere`);
    assert.equal(notFound.status, 404);
    assert.equal(((await notFound.json()) as any).error.code, 'route_not_found');
    const wrongMethod = await fetch(`${gw.url}/exact`);
    assert.equal(wrongMethod.status, 405);
    assert.equal(wrongMethod.headers.get('allow'), 'POST');
    const traversal = await rawRequest(gw.url, '/ai/%2e%2e/secret');
    assert.equal(traversal.status, 400);
    const absolute = await rawRequest(gw.url, 'http://elsewhere/ai/x');
    assert.equal(absolute.status, 400);
  });

  it('enforces the server body limit', async () => {
    const res = await postJson(`${gw.url}/ai/x`, { blob: 'x'.repeat(2000) });
    assert.equal(res.status, 413);
  });

  it('round-robins between upstreams', async () => {
    const before1 = up.requests.length;
    const before2 = up2.requests.length;
    for (let i = 0; i < 4; i++) await (await fetch(`${gw.url}/rr/ping`)).text();
    assert.equal(up.requests.length - before1, 2);
    assert.equal(up2.requests.length - before2, 2);
  });

  it('records stats per route, principal and model', async () => {
    await postJson(`${gw.url}/ai/chat`, { model: 'stats-model' }, bearer(AGENT_KEY));
    const snap = gw.gateway.stats.snapshot();
    assert.ok(snap.routes.ai!.requests >= 1);
    assert.equal(snap.models['stats-model']!.totalTokens, 15);
    assert.equal(snap.principals['key:agent']!.requests, 1);
  });
});

describe('failover and errors', () => {
  let good: MockUpstream;
  let flaky: MockUpstream;
  let slow: MockUpstream;
  let gw: TestGateway;
  const deadUrl = 'http://127.0.0.1:1';

  before(async () => {
    good = await startUpstream();
    flaky = await startUpstream((_req, res) => {
      res.writeHead(503, { 'content-type': 'application/json' });
      res.end('{"error":"overloaded"}');
    });
    slow = await startUpstream(() => {
      /* never responds */
    });
    gw = await startGateway(`
      <moxy>
        <routes>
          <route id="dead-then-good" path="/a/*"><upstream url="${deadUrl}"/><upstream url="${good.url}"/></route>
          <route id="flaky-then-good" path="/b/*"><upstream url="${flaky.url}"/><upstream url="${good.url}"/></route>
          <route id="no-retry-on-503" path="/c/*" retry-on="error"><upstream url="${flaky.url}"/><upstream url="${good.url}"/></route>
          <route id="all-dead" path="/d/*"><upstream url="${deadUrl}"/><upstream url="http://127.0.0.1:2"/></route>
          <route id="slow" path="/e/*"><upstream url="${slow.url}"/><middleware><timeout value="200ms"/></middleware></route>
        </routes>
      </moxy>`);
  });
  after(async () => {
    await gw.close();
    await Promise.all([good.close(), flaky.close(), slow.close()]);
  });

  it('fails over on connection errors and marks the upstream unhealthy', async () => {
    const res = await fetch(`${gw.url}/a/x`);
    assert.equal(res.status, 200);
    assert.equal(((await res.json()) as any).url, '/x');
    assert.equal(gw.gateway.health.get(deadUrl).failures, 1);
    assert.ok(gw.gateway.health.get(deadUrl).lastError);
  });

  it('fails over on retryable status codes', async () => {
    const res = await fetch(`${gw.url}/b/x`);
    assert.equal(res.status, 200);
  });

  it('relays upstream errors that are not retryable', async () => {
    const res = await fetch(`${gw.url}/c/x`);
    assert.equal(res.status, 503);
    assert.equal(await res.text(), '{"error":"overloaded"}');
  });

  it('returns 502 when every upstream fails', async () => {
    const res = await fetch(`${gw.url}/d/x`);
    assert.equal(res.status, 502);
    assert.equal(((await res.json()) as any).error.code, 'upstream_unavailable');
  });

  it('returns 504 on upstream timeout', async () => {
    const started = Date.now();
    const res = await fetch(`${gw.url}/e/x`);
    assert.equal(res.status, 504);
    assert.equal(((await res.json()) as any).error.code, 'upstream_timeout');
    assert.ok(Date.now() - started < 2000);
  });
});

describe('streaming', () => {
  let up: MockUpstream;
  let gw: TestGateway;
  let upstreamClosed: Promise<void>;
  let markClosed: () => void;

  before(async () => {
    up = await startUpstream((req, res) => {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write('data: {"choices":[{"delta":{"content":"Hel"}}]}\n\n');
      if (req.url?.includes('forever')) {
        const timer = setInterval(() => res.write(': keep-alive\n\n'), 50);
        res.on('close', () => {
          clearInterval(timer);
          markClosed();
        });
        return;
      }
      setTimeout(() => {
        res.write('data: {"choices":[{"delta":{"content":"lo"}}]}\n\n');
        res.write('data: {"model":"m1","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}\n\n');
        res.end('data: [DONE]\n\n');
      }, 300);
    });
    gw = await startGateway(`
      <moxy><routes>
        <route id="sse" path="/sse/*"><upstream url="${up.url}"/><middleware><cache/></middleware></route>
      </routes></moxy>`);
  });
  after(async () => {
    await gw.close();
    await up.close();
  });

  it('relays SSE chunks as they arrive and records usage', async () => {
    const started = Date.now();
    const res = await postJson(`${gw.url}/sse/chat`, { model: 'm1', stream: true });
    assert.equal(res.headers.get('content-type'), 'text/event-stream');
    const reader = res.body!.getReader();
    const first = await reader.read();
    assert.ok(Date.now() - started < 250, 'first chunk should arrive before the upstream finishes');
    assert.match(Buffer.from(first.value!).toString(), /Hel/);
    let rest = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      rest += Buffer.from(value).toString();
    }
    assert.match(rest, /\[DONE\]/);
    await new Promise((r) => setTimeout(r, 20));
    const stats = gw.gateway.stats.snapshot();
    assert.equal(stats.routes.sse!.totalTokens, 9);
    assert.equal(stats.models.m1!.promptTokens, 7);
  });

  it('closes the upstream connection when the client disconnects', async () => {
    upstreamClosed = new Promise((resolve) => (markClosed = resolve));
    await new Promise<void>((resolve, reject) => {
      const req = http.request(`${gw.url}/sse/forever`, { method: 'POST', headers: { 'content-type': 'application/json' } }, (res) => {
        res.once('data', () => {
          req.destroy();
          resolve();
        });
      });
      req.on('error', () => {});
      req.end('{}');
      setTimeout(() => reject(new Error('no data')), 2000);
    });
    await Promise.race([upstreamClosed, new Promise((_, reject) => setTimeout(() => reject(new Error('upstream not closed')), 2000))]);
  });
});
