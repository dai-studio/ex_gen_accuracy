import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { hasDotSegments } from '../src/gateway.ts';
import { SlidingWindowLimiter } from '../src/middleware/helpers.ts';
import { buildTargetUrl, shouldRetry, upstreamOrder, UpstreamHealth } from '../src/proxy.ts';
import { Router } from '../src/router.ts';
import { extractUsage, mergeUsage, scanBody, UsageScanner } from '../src/usage.ts';
import {
  getJsonPath,
  globToRegExp,
  parseBool,
  parseCookies,
  parseDuration,
  parseSize,
  removeCookie,
  safeNextPath,
  setJsonPath,
  splitList,
} from '../src/util.ts';
import type { CompiledRoute } from '../src/middleware/types.ts';

describe('util', () => {
  it('parses durations', () => {
    assert.equal(parseDuration('250ms'), 250);
    assert.equal(parseDuration('30s'), 30_000);
    assert.equal(parseDuration('5m'), 300_000);
    assert.equal(parseDuration('1.5h'), 5_400_000);
    assert.equal(parseDuration('1d'), 86_400_000);
    assert.equal(parseDuration('10'), 10_000);
    assert.throws(() => parseDuration('soon'));
  });

  it('parses sizes', () => {
    assert.equal(parseSize('512'), 512);
    assert.equal(parseSize('64kb'), 65_536);
    assert.equal(parseSize('10MB'), 10 * 1024 * 1024);
    assert.equal(parseSize('1g'), 1024 ** 3);
    assert.throws(() => parseSize('lots'));
  });

  it('parses booleans and lists', () => {
    assert.equal(parseBool(undefined, true), true);
    assert.equal(parseBool('yes', false), true);
    assert.equal(parseBool('off', true), false);
    assert.throws(() => parseBool('maybe', true));
    assert.deepEqual(splitList(' a, b  c,,d '), ['a', 'b', 'c', 'd']);
    assert.deepEqual(splitList(undefined), []);
  });

  it('matches globs', () => {
    assert.ok(globToRegExp('gpt-4*').test('gpt-4o-mini'));
    assert.ok(!globToRegExp('gpt-4*').test('xgpt-4'));
    assert.ok(globToRegExp('claude-?-*').test('claude-3-opus'));
    assert.ok(globToRegExp('a.b').test('a.b'));
    assert.ok(!globToRegExp('a.b').test('axb'));
  });

  it('gets and sets JSON paths', () => {
    const obj: Record<string, unknown> = { a: { b: [1, { c: 2 }] } };
    assert.equal(getJsonPath(obj, 'a.b.1.c'), 2);
    assert.equal(getJsonPath(obj, 'a.x.y'), undefined);
    setJsonPath(obj, 'a.d.e', 5);
    assert.equal(getJsonPath(obj, 'a.d.e'), 5);
    assert.throws(() => setJsonPath(obj, '__proto__.polluted', 1));
  });

  it('handles cookies', () => {
    assert.deepEqual(parseCookies('a=1; b="two"; c=%20x'), { a: '1', b: 'two', c: ' x' });
    assert.equal(removeCookie('a=1; moxy=2; b=3', 'moxy'), 'a=1; b=3');
    assert.equal(removeCookie('moxy=2', 'moxy'), undefined);
  });

  it('only allows local redirect targets', () => {
    assert.equal(safeNextPath('/chat', '/'), '/chat');
    assert.equal(safeNextPath('//evil.com', '/'), '/');
    assert.equal(safeNextPath('https://evil.com', '/'), '/');
    assert.equal(safeNextPath('/\\evil.com', '/'), '/');
    assert.equal(safeNextPath(null, '/x'), '/x');
  });

  it('detects dot segments, including encoded ones', () => {
    assert.ok(hasDotSegments('/a/../b'));
    assert.ok(hasDotSegments('/a/%2e%2e/b'));
    assert.ok(hasDotSegments('/a/..%2Fb'));
    assert.ok(hasDotSegments('/a/./b'));
    assert.ok(!hasDotSegments('/a/b..c/d'));
    assert.ok(!hasDotSegments('/v1/chat/completions'));
  });
});

describe('Router', () => {
  const routes = [
    { id: 'catch', path: '/*', methods: null },
    { id: 'openai', path: '/openai/*', methods: null },
    { id: 'chat', path: '/openai/v1/chat', methods: ['POST'] },
    { id: 'deep', path: '/openai/v1/*', methods: ['GET'] },
  ];
  const router = new Router(routes);
  const match = (method: string, path: string) => {
    const r = router.match(method, path);
    return r.kind === 'match' ? `${r.match.route.id}:${r.match.remainder}` : r.kind;
  };

  it('prefers exact routes, then longer prefixes', () => {
    assert.equal(match('POST', '/openai/v1/chat'), 'chat:');
    assert.equal(match('GET', '/openai/v1/models'), 'deep:/models');
    assert.equal(match('GET', '/openai/files'), 'openai:/files');
    assert.equal(match('GET', '/openai'), 'openai:');
    assert.equal(match('GET', '/elsewhere'), 'catch:/elsewhere');
  });

  it('falls through to less specific routes when methods do not match', () => {
    assert.equal(match('GET', '/openai/v1/chat'), 'deep:/chat');
    assert.equal(match('POST', '/openai/v1/models'), 'openai:/v1/models');
  });

  it('reports 405 with allowed methods when nothing accepts the method', () => {
    const r = new Router([{ path: '/x', methods: ['POST'] }]).match('GET', '/x');
    assert.deepEqual(r, { kind: 'method-not-allowed', allowed: ['POST'] });
  });

  it('lets OPTIONS and HEAD through', () => {
    const r = new Router([{ path: '/x', methods: ['GET'] }]);
    assert.equal(r.match('OPTIONS', '/x').kind, 'match');
    assert.equal(r.match('HEAD', '/x').kind, 'match');
  });

  it('does not match partial segments', () => {
    const r = new Router([{ path: '/api2/*', methods: null }]);
    assert.equal(r.match('GET', '/api2x').kind, 'none');
  });
});

describe('proxy helpers', () => {
  it('joins upstream URLs', () => {
    assert.equal(buildTargetUrl('https://h/v1', '/chat', '?a=1'), 'https://h/v1/chat?a=1');
    assert.equal(buildTargetUrl('https://h/v1/', '/chat', ''), 'https://h/v1/chat');
    assert.equal(buildTargetUrl('https://h/exact/path', '', '?q'), 'https://h/exact/path?q');
  });

  it('evaluates retry-on rules', () => {
    const route = { retryOn: ['error', '5xx', '429'] };
    assert.ok(shouldRetry(route, 'error'));
    assert.ok(shouldRetry(route, 503));
    assert.ok(shouldRetry(route, 429));
    assert.ok(!shouldRetry(route, 404));
    assert.ok(!shouldRetry({ retryOn: ['502'] }, 'error'));
  });

  const mkRoute = (strategy: CompiledRoute['strategy'], weights: number[]): CompiledRoute =>
    ({
      strategy,
      cursor: 0,
      upstreams: weights.map((weight, i) => ({ url: `http://u${i}`, weight, headers: [] })),
    }) as unknown as CompiledRoute;

  it('orders upstreams for failover, pushing unhealthy ones last', () => {
    const health = new UpstreamHealth(1, 60_000);
    const route = mkRoute('failover', [1, 1, 1]);
    assert.deepEqual(upstreamOrder(route, health).map((u) => u.url), ['http://u0', 'http://u1', 'http://u2']);
    health.failure('http://u0', 'boom');
    assert.deepEqual(upstreamOrder(route, health).map((u) => u.url), ['http://u1', 'http://u2', 'http://u0']);
    health.success('http://u0');
    assert.equal(upstreamOrder(route, health)[0]!.url, 'http://u0');
  });

  it('rotates weighted round-robin', () => {
    const health = new UpstreamHealth();
    const route = mkRoute('round-robin', [2, 1]);
    const firsts = Array.from({ length: 6 }, () => upstreamOrder(route, health)[0]!.url);
    assert.deepEqual(firsts, ['http://u0', 'http://u0', 'http://u1', 'http://u0', 'http://u0', 'http://u1']);
  });

  it('picks weighted random', () => {
    const route = mkRoute('random', [1, 3]);
    const health = new UpstreamHealth();
    assert.equal(upstreamOrder(route, health, () => 0.1)[0]!.url, 'http://u0');
    assert.equal(upstreamOrder(route, health, () => 0.5)[0]!.url, 'http://u1');
  });
});

describe('SlidingWindowLimiter', () => {
  it('limits within a window and recovers gradually', () => {
    const l = new SlidingWindowLimiter(3, 1000);
    const t0 = 10_000;
    assert.ok(l.hit('k', t0).allowed);
    assert.ok(l.hit('k', t0 + 1).allowed);
    const third = l.hit('k', t0 + 2);
    assert.ok(third.allowed);
    assert.equal(third.remaining, 0);
    assert.ok(!l.hit('k', t0 + 3).allowed);
    assert.ok(l.hit('other', t0 + 3).allowed);
    // Just into the next window the previous count still weighs ~100%.
    assert.ok(!l.hit('k', t0 + 1001).allowed);
    // Two-thirds through the next window, only a third of the old count remains.
    assert.ok(l.hit('k', t0 + 1700).allowed);
    // Estimate is now 0.3 * 3 + 1 = 1.9; one more hit fits, after which a third would not.
    assert.equal(l.wouldAllow('k', t0 + 1700), true);
    assert.ok(l.hit('k', t0 + 1700).allowed);
    assert.equal(l.wouldAllow('k', t0 + 1701), false);
    assert.equal(l.wouldAllow('fresh', t0), true);
    // Two windows later the bucket is clear.
    assert.equal(l.hit('k', t0 + 3500).remaining, 2);
  });
});

describe('usage extraction', () => {
  it('reads OpenAI, Anthropic and Ollama shapes', () => {
    assert.deepEqual(extractUsage({ usage: { prompt_tokens: 3, completion_tokens: 4, total_tokens: 7 } }), {
      promptTokens: 3,
      completionTokens: 4,
      totalTokens: 7,
    });
    assert.deepEqual(extractUsage({ usage: { input_tokens: 5, output_tokens: 6 } }), { promptTokens: 5, completionTokens: 6, totalTokens: 11 });
    assert.deepEqual(extractUsage({ type: 'message_start', message: { usage: { input_tokens: 9, output_tokens: 1 } } }), {
      promptTokens: 9,
      completionTokens: 1,
      totalTokens: 10,
    });
    assert.deepEqual(extractUsage({ prompt_eval_count: 2, eval_count: 8 }), { promptTokens: 2, completionTokens: 8, totalTokens: 10 });
    assert.equal(extractUsage({ choices: [] }), null);
  });

  it('merges cumulative streaming counts', () => {
    const merged = mergeUsage({ promptTokens: 9, completionTokens: 1, totalTokens: 10 }, { promptTokens: 0, completionTokens: 25, totalTokens: 25 });
    assert.deepEqual(merged, { promptTokens: 9, completionTokens: 25, totalTokens: 34 });
  });

  it('scans SSE streams split across chunks', () => {
    const s = new UsageScanner('text/event-stream');
    const stream =
      'event: message_start\ndata: {"type":"message_start","message":{"model":"claude-x","usage":{"input_tokens":12,"output_tokens":1}}}\n\n' +
      'data: {"type":"content_block_delta"}\n\n' +
      'event: message_delta\ndata: {"type":"message_delta","usage":{"output_tokens":30}}\n\n' +
      'data: [DONE]\n\n';
    const bytes = Buffer.from(stream);
    for (let i = 0; i < bytes.length; i += 7) s.push(bytes.subarray(i, i + 7));
    s.end();
    assert.deepEqual(s.usage, { promptTokens: 12, completionTokens: 30, totalTokens: 42 });
    assert.equal(s.model, 'claude-x');
  });

  it('scans JSON and NDJSON bodies', () => {
    assert.equal(scanBody('application/json', Buffer.from('{"model":"m","usage":{"total_tokens":4}}')).usage!.totalTokens, 4);
    const nd = scanBody('application/x-ndjson', Buffer.from('{"response":"a"}\n{"done":true,"prompt_eval_count":3,"eval_count":4}\n'));
    assert.equal(nd.usage!.totalTokens, 7);
    assert.equal(scanBody('text/plain', Buffer.from('{"usage":{"total_tokens":4}}')).usage, null);
  });
});
