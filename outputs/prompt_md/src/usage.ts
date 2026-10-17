import { StringDecoder } from 'node:string_decoder';
import { Transform, type TransformCallback } from 'node:stream';

export interface Usage {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

const num = (v: unknown): number | undefined => (typeof v === 'number' && Number.isFinite(v) ? v : undefined);

/**
 * Pulls token usage out of one response object. Understands OpenAI
 * (`usage.prompt_tokens`...), Anthropic (`usage.input_tokens`..., including
 * `message_start` events) and Ollama (`prompt_eval_count`/`eval_count`).
 */
export function extractUsage(obj: unknown): Usage | null {
  if (!obj || typeof obj !== 'object') return null;
  const o = obj as Record<string, any>;
  const u = (o.usage ?? o.message?.usage ?? o.response?.usage) as Record<string, unknown> | undefined;
  let prompt: number | undefined;
  let completion: number | undefined;
  let total: number | undefined;
  if (u && typeof u === 'object') {
    prompt = num(u.prompt_tokens) ?? num(u.input_tokens);
    completion = num(u.completion_tokens) ?? num(u.output_tokens);
    total = num(u.total_tokens);
  } else {
    prompt = num(o.prompt_eval_count);
    completion = num(o.eval_count);
  }
  if (prompt === undefined && completion === undefined && total === undefined) return null;
  const p = prompt ?? 0;
  const c = completion ?? 0;
  return { promptTokens: p, completionTokens: c, totalTokens: total ?? p + c };
}

/** Streaming events report cumulative counts, so merging keeps the maximum of each field. */
export function mergeUsage(a: Usage | null, b: Usage | null): Usage | null {
  if (!a) return b;
  if (!b) return a;
  const promptTokens = Math.max(a.promptTokens, b.promptTokens);
  const completionTokens = Math.max(a.completionTokens, b.completionTokens);
  return { promptTokens, completionTokens, totalTokens: Math.max(a.totalTokens, b.totalTokens, promptTokens + completionTokens) };
}

type Mode = 'sse' | 'ndjson' | 'json' | 'none';

const MAX_JSON_BYTES = 8 * 1024 * 1024;

function modeFor(contentType: string | undefined): Mode {
  const ct = (contentType ?? '').toLowerCase();
  if (ct.includes('text/event-stream')) return 'sse';
  if (ct.includes('ndjson') || ct.includes('jsonl')) return 'ndjson';
  if (ct.includes('json')) return 'json';
  return 'none';
}

/** Incrementally observes a response body and records usage and model name. */
export class UsageScanner {
  usage: Usage | null = null;
  model?: string;
  private readonly mode: Mode;
  private readonly decoder = new StringDecoder('utf8');
  private pending = '';
  private jsonChunks: Buffer[] = [];
  private jsonSize = 0;

  constructor(contentType: string | undefined) {
    this.mode = modeFor(contentType);
  }

  push(chunk: Buffer): void {
    if (this.mode === 'none') return;
    if (this.mode === 'json') {
      if (this.jsonSize + chunk.length <= MAX_JSON_BYTES) this.jsonChunks.push(chunk);
      this.jsonSize += chunk.length;
      return;
    }
    this.pending += this.decoder.write(chunk);
    let nl: number;
    while ((nl = this.pending.indexOf('\n')) >= 0) {
      this.line(this.pending.slice(0, nl).replace(/\r$/, ''));
      this.pending = this.pending.slice(nl + 1);
    }
  }

  end(): void {
    if (this.mode === 'json') {
      if (this.jsonSize <= MAX_JSON_BYTES) this.observe(Buffer.concat(this.jsonChunks).toString('utf8'));
      this.jsonChunks = [];
    } else if (this.mode !== 'none') {
      this.pending += this.decoder.end();
      if (this.pending) this.line(this.pending);
      this.pending = '';
    }
  }

  private line(line: string): void {
    if (this.mode === 'sse') {
      if (!line.startsWith('data:')) return;
      const data = line.slice(5).trim();
      if (data && data !== '[DONE]') this.observe(data);
    } else if (line.trim()) {
      this.observe(line);
    }
  }

  private observe(text: string): void {
    let obj: unknown;
    try {
      obj = JSON.parse(text);
    } catch {
      return;
    }
    if (!obj || typeof obj !== 'object') return;
    this.usage = mergeUsage(this.usage, extractUsage(obj));
    const o = obj as Record<string, any>;
    const model = typeof o.model === 'string' ? o.model : typeof o.message?.model === 'string' ? o.message.model : undefined;
    if (model && !this.model) this.model = model;
  }
}

/** Pass-through stream that feeds a UsageScanner. */
export class UsageTap extends Transform {
  readonly scanner: UsageScanner;

  constructor(contentType: string | undefined) {
    super();
    this.scanner = new UsageScanner(contentType);
  }

  override _transform(chunk: Buffer, _enc: BufferEncoding, cb: TransformCallback): void {
    this.scanner.push(chunk);
    cb(null, chunk);
  }

  override _flush(cb: TransformCallback): void {
    this.scanner.end();
    cb();
  }
}

export function scanBody(contentType: string | undefined, body: Buffer): UsageScanner {
  const s = new UsageScanner(contentType);
  s.push(body);
  s.end();
  return s;
}
