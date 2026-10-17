import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { ConfigError, interpolate, parseConfig, sha256Hex } from '../src/config.ts';
import { buildRuntime } from '../src/runtime.ts';
import { RevocationList } from '../src/auth.ts';
import { Logger } from '../src/logger.ts';

const route = (inner = '<upstream url="http://localhost:1"/>', attrs = 'path="/x/*"') => `<moxy><routes><route ${attrs}>${inner}</route></routes></moxy>`;

function expectError(xml: string, pattern: RegExp, env: Record<string, string> = {}): void {
  assert.throws(
    () => {
      const config = parseConfig(xml, { env });
      buildRuntime(config, { logger: new Logger('silent'), secret: 'x'.repeat(32), revocations: new RevocationList() });
    },
    (err: unknown) => err instanceof ConfigError && pattern.test(err.message),
  );
}

describe('interpolate', () => {
  it('expands variables, defaults and escapes', () => {
    const env = { A: 'alpha', EMPTY: '' };
    assert.equal(interpolate('x-${A}-y', env), 'x-alpha-y');
    assert.equal(interpolate('${MISSING:-fallback}', env), 'fallback');
    assert.equal(interpolate('${EMPTY:-fallback}', env), 'fallback');
    assert.equal(interpolate('${MISSING:-}', env), '');
    assert.equal(interpolate('$${A}', env), '${A}');
    assert.throws(() => interpolate('${MISSING}', env), /MISSING is not set/);
  });
});

describe('parseConfig', () => {
  it('applies defaults for a minimal config', () => {
    const c = parseConfig('<moxy/>');
    assert.equal(c.server.host, '127.0.0.1');
    assert.equal(c.server.port, 8080);
    assert.equal(c.server.maxBodyBytes, 10 * 1024 * 1024);
    assert.equal(c.logging.level, 'info');
    assert.equal(c.auth.cookieName, 'moxy_session');
    assert.equal(c.builtins.admin, true);
    assert.deepEqual(c.routes, []);
  });

  it('parses a full configuration', () => {
    const c = parseConfig(
      `<moxy>
        <server host="0.0.0.0" port="\${PORT}" trust-proxy="true" max-body="2mb"/>
        <logging level="debug" access="false" format="text"/>
        <auth secret="0123456789abcdef0123" token-ttl="30m" cookie="sess">
          <user name="a" password="pw" roles="admin, developer"/>
          <api-key name="k" key-hash="sha256:${'ab'.repeat(32)}" roles="agent"/>
        </auth>
        <builtins chat="false" title="Gate"/>
        <upstreams>
          <upstream name="up" url="https://api.example.com/v1" timeout="45s" weight="2">
            <header name="Authorization" value="Bearer \${KEY}"/>
          </upstream>
        </upstreams>
        <chains><chain name="c"><auth/><rate-limit requests="5"/></chain></chains>
        <middleware><cors/></middleware>
        <routes>
          <route id="r1" path="/ai/*" methods="get, post" strategy="round-robin" retries="3" retry-on="error,5xx,429">
            <description>
              The   AI route
            </description>
            <upstream ref="up"/>
            <upstream url="http://backup:8000/v1"><header name="X-Extra" value="1"/></upstream>
            <models chat-path="/chat" embeddings-path="/embed">
              <model id="fast" upstream-model="gpt-4o-mini" description="quick"/>
              <model id="gpt-4o"/>
            </models>
            <middleware><use chain="c"/><timeout value="10s"/></middleware>
          </route>
        </routes>
      </moxy>`,
      { env: { PORT: '9000', KEY: 'sk-123' } },
    );
    assert.equal(c.server.port, 9000);
    assert.equal(c.server.trustProxy, true);
    assert.equal(c.server.maxBodyBytes, 2 * 1024 * 1024);
    assert.deepEqual(c.logging, { level: 'debug', access: false, format: 'text' });
    assert.equal(c.auth.tokenTtlMs, 30 * 60_000);
    assert.deepEqual(c.auth.users[0]!.roles, ['admin', 'developer']);
    assert.equal(c.auth.apiKeys[0]!.keyHash, 'ab'.repeat(32));
    assert.equal(c.builtins.chat, false);
    assert.equal(c.builtins.title, 'Gate');
    assert.deepEqual(c.globalMiddleware.map((m) => m.type), ['cors']);
    const r = c.routes[0]!;
    assert.equal(r.id, 'r1');
    assert.deepEqual(r.methods, ['GET', 'POST']);
    assert.equal(r.description, 'The AI route');
    assert.equal(r.strategy, 'round-robin');
    assert.equal(r.retries, 3);
    assert.deepEqual(r.retryOn, ['error', '5xx', '429']);
    assert.equal(r.upstreams[0]!.name, 'up');
    assert.equal(r.upstreams[0]!.weight, 2);
    assert.equal(r.upstreams[0]!.timeoutMs, 45_000);
    assert.deepEqual(r.upstreams[0]!.headers, [['authorization', 'Bearer sk-123']]);
    assert.deepEqual(r.upstreams[1]!.headers, [['x-extra', '1']]);
    assert.deepEqual(r.middleware.map((m) => m.type), ['auth', 'rate-limit', 'timeout']);
    assert.equal(r.models!.chatPath, '/chat');
    assert.deepEqual(r.models!.models[0], { id: 'fast', upstreamModel: 'gpt-4o-mini', description: 'quick' });
  });

  it('hashes plain api keys and defaults retries to upstream count - 1', () => {
    const c = parseConfig(
      `<moxy><auth><api-key name="k" key="0123456789abcdefXYZ"/></auth>
       <routes><route path="/a"><upstream url="http://a"/><upstream url="http://b"/><upstream url="http://c"/></route></routes></moxy>`,
    );
    assert.equal(c.auth.apiKeys[0]!.keyHash, sha256Hex('0123456789abcdefXYZ'));
    assert.equal(c.routes[0]!.retries, 2);
    assert.equal(c.routes[0]!.id, 'route-1');
    assert.equal(c.routes[0]!.methods, null);
  });

  describe('validation errors', () => {
    const cases: Array<[string, string, RegExp]> = [
      ['wrong root', '<config/>', /Root element must be <moxy>/],
      ['malformed xml', '<moxy>', /Malformed XML/],
      ['unknown section', '<moxy><bogus/></moxy>', /Unknown element <bogus>/],
      ['duplicate section', '<moxy><server/><server/></moxy>', /may only appear once/],
      ['unknown attribute', '<moxy><server prot="1"/></moxy>', /Unknown attribute "prot" on <server>/],
      ['bad port', '<moxy><server port="99999"/></moxy>', /Invalid port/],
      ['short secret', '<moxy><auth secret="short"/></moxy>', /at least 16 characters/],
      ['user without password', '<moxy><auth><user name="a"/></auth></moxy>', /exactly one of password or password-hash/],
      ['bad hash', '<moxy><auth><user name="a" password-hash="md5$x"/></auth></moxy>', /malformed password-hash/],
      ['short key', '<moxy><auth><api-key name="k" key="short"/></auth></moxy>', /at least 16 characters/],
      ['no upstream', route(''), /needs at least one <upstream>/],
      ['bad upstream url', route('<upstream url="ftp://x"/>'), /must be http or https/],
      ['unknown upstream ref', route('<upstream ref="nope"/>'), /Unknown upstream "nope"/],
      ['relative path', route(undefined, 'path="x"'), /must start with "\/"/],
      ['star in middle', route(undefined, 'path="/a/*/b"'), /only allowed as a trailing/],
      ['builtin collision', route(undefined, 'path="/admin/stuff"'), /collides with the built-in admin/],
      ['healthz collision', route(undefined, 'path="/healthz"'), /collides with the built-in \/healthz/],
      ['bad strategy', route(undefined, 'path="/a" strategy="fastest"'), /Invalid strategy/],
      ['bad retry-on', route(undefined, 'path="/a" retry-on="sometimes"'), /Invalid retry-on/],
      ['unknown middleware', route('<upstream url="http://a"/><middleware><bogus/></middleware>'), /Unknown middleware <bogus>/],
      ['unknown chain', route('<upstream url="http://a"/><middleware><use chain="x"/></middleware>'), /Unknown chain "x"/],
      ['missing middleware attr', route('<upstream url="http://a"/><middleware><rate-limit/></middleware>'), /requires a "requests" attribute/],
      ['bad middleware attr', route('<upstream url="http://a"/><middleware><rate-limit requests="5" burst="1"/></middleware>'), /Unknown attribute "burst"/],
      ['bad duration', route('<upstream url="http://a"/><middleware><timeout value="soon"/></middleware>'), /Invalid duration/],
      ['bad redact pattern', route('<upstream url="http://a"/><middleware><redact patterns="dna"/></middleware>'), /unknown pattern "dna"/],
      ['missing env', '<moxy><server host="${NOPE}"/></moxy>', /NOPE is not set/],
    ];
    for (const [name, xml, pattern] of cases) {
      it(name, () => expectError(xml, pattern));
    }

    it('detects duplicate route ids', () => {
      expectError(
        '<moxy><routes><route id="a" path="/a"><upstream url="http://a"/></route><route id="a" path="/b"><upstream url="http://a"/></route></routes></moxy>',
        /Duplicate route id "a"/,
      );
    });

    it('detects chain cycles', () => {
      expectError(
        `<moxy><chains><chain name="a"><use chain="b"/></chain><chain name="b"><use chain="a"/></chain></chains>
         <routes><route path="/a"><upstream url="http://a"/><middleware><use chain="a"/></middleware></route></routes></moxy>`,
        /includes itself/,
      );
    });

    it('allows builtin paths once the builtin is disabled', () => {
      const c = parseConfig('<moxy><builtins admin="false"/><routes><route path="/admin/*"><upstream url="http://a"/></route></routes></moxy>');
      assert.equal(c.routes[0]!.path, '/admin/*');
    });

    it('includes the line number', () => {
      try {
        parseConfig('<moxy>\n\n  <server port="x"/>\n</moxy>');
        assert.fail('should throw');
      } catch (err) {
        assert.ok(err instanceof ConfigError);
        assert.equal(err.line, 3);
      }
    });
  });
});
