import http, { type IncomingMessage, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { parseConfig } from '../src/config.ts';
import { Gateway } from '../src/gateway.ts';
import { Logger } from '../src/logger.ts';

export interface RecordedRequest {
  method: string;
  url: string;
  headers: IncomingMessage['headers'];
  body: string;
  json?: any;
}

export interface MockUpstream {
  url: string;
  port: number;
  requests: RecordedRequest[];
  /** Replace to customise responses; defaults to echoing the request as JSON with usage. */
  handler: (req: RecordedRequest, res: ServerResponse) => void;
  close(): Promise<void>;
}

export function echoHandler(req: RecordedRequest, res: ServerResponse): void {
  const body = JSON.stringify({
    method: req.method,
    url: req.url,
    headers: req.headers,
    body: req.json ?? req.body,
    model: req.json?.model,
    usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 },
  });
  res.writeHead(200, { 'content-type': 'application/json', 'x-upstream': 'yes', 'x-request-id': 'upstream-id' });
  res.end(body);
}

export async function startUpstream(handler = echoHandler): Promise<MockUpstream> {
  const requests: RecordedRequest[] = [];
  const sockets = new Set<import('node:net').Socket>();
  const mock: MockUpstream = {
    url: '',
    port: 0,
    requests,
    handler,
    close: () =>
      new Promise((resolve) => {
        for (const s of sockets) s.destroy();
        server.close(() => resolve());
      }),
  };
  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (c: Buffer) => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks).toString('utf8');
      let json: any;
      try {
        json = body ? JSON.parse(body) : undefined;
      } catch {
        /* not json */
      }
      const recorded = { method: req.method ?? '', url: req.url ?? '', headers: req.headers, body, json };
      requests.push(recorded);
      mock.handler(recorded, res);
    });
  });
  server.on('connection', (s) => {
    sockets.add(s);
    s.on('close', () => sockets.delete(s));
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  mock.port = (server.address() as AddressInfo).port;
  mock.url = `http://127.0.0.1:${mock.port}`;
  return mock;
}

export interface TestGateway {
  gateway: Gateway;
  url: string;
  close(): Promise<void>;
}

export async function startGateway(xml: string, env: Record<string, string> = {}, configPath?: string): Promise<TestGateway> {
  const config = parseConfig(xml, { env });
  const gateway = new Gateway({ config, configPath, logger: new Logger('silent') });
  const address = await gateway.listen(0, '127.0.0.1');
  return { gateway, url: `http://127.0.0.1:${address.port}`, close: () => gateway.close(100) };
}

export const SECRET = 'test-secret-0123456789abcdef';
export const AGENT_KEY = 'agent-key-0123456789abcdef';
export const DEV_KEY = 'dev-key-0123456789abcdef';
export const ADMIN_KEY = 'admin-key-0123456789abcdef';

/** Standard <auth> block used by most gateway tests. */
export const AUTH_XML = `
  <auth secret="${SECRET}" token-ttl="1h">
    <user name="admin" password="admin-pass" roles="admin"/>
    <user name="dev" password="dev-pass" roles="developer"/>
    <api-key name="agent" key="${AGENT_KEY}" roles="agent"/>
    <api-key name="devkey" key="${DEV_KEY}" roles="developer"/>
    <api-key name="adminkey" key="${ADMIN_KEY}" roles="admin"/>
  </auth>`;

export function bearer(token: string): Record<string, string> {
  return { authorization: `Bearer ${token}` };
}

export async function postJson(url: string, body: unknown, headers: Record<string, string> = {}): Promise<Response> {
  return fetch(url, { method: 'POST', headers: { 'content-type': 'application/json', ...headers }, body: JSON.stringify(body) });
}

export async function login(base: string, username: string, password: string): Promise<{ token: string; cookie: string }> {
  const res = await postJson(`${base}/auth/login`, { username, password });
  if (res.status !== 200) throw new Error(`login failed: ${res.status}`);
  const body = (await res.json()) as { token: string };
  const cookie = res.headers.get('set-cookie')!.split(';')[0]!;
  return { token: body.token, cookie };
}

/** Low-level request that sends the path and headers exactly as given (fetch normalises both). */
export function rawRequest(
  base: string,
  path: string,
  options: { method?: string; headers?: Record<string, string>; body?: string } = {},
): Promise<{ status: number; headers: http.IncomingHttpHeaders; body: string }> {
  return new Promise((resolve, reject) => {
    const u = new URL(base);
    const req = http.request({ host: u.hostname, port: u.port, path, method: options.method ?? 'GET', headers: options.headers }, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (c: Buffer) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode ?? 0, headers: res.headers, body: Buffer.concat(chunks).toString('utf8') }));
    });
    req.on('error', reject);
    req.end(options.body);
  });
}
