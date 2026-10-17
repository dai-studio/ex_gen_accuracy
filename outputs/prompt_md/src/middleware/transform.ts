import { checkAttrs, ConfigError } from '../config.ts';
import type { Context } from '../context.ts';
import { globToRegExp, matchesAny, parseBool, parseDuration, parseIntStrict, setJsonPath, getJsonPath, splitList } from '../util.ts';
import { attrs, expandTemplate, oneOf, parsed, requireAttr } from './helpers.ts';
import { errorReply, type MiddlewareFactory } from './types.ts';

const HEADER_NAME = /^[A-Za-z0-9!#$%&'*+.^_`|~-]+$/;
const PROTECTED_HEADERS = new Set(['host', 'content-length', 'transfer-encoding', 'connection']);

function headerName(def: Parameters<MiddlewareFactory>[0], raw: string): string {
  const name = raw.trim().toLowerCase();
  if (!HEADER_NAME.test(name)) throw new ConfigError(`<${def.type}> invalid header name "${raw}"`, def.element);
  if (PROTECTED_HEADERS.has(name)) throw new ConfigError(`<${def.type}> cannot modify the ${name} header`, def.element);
  return name;
}

/**
 * <set-header name="X-User" value="{principal.name}" phase="request|response" override="true"/>
 */
export const setHeader: MiddlewareFactory = (def) => {
  const a = attrs(def, ['name', 'value', 'phase', 'override']);
  const name = headerName(def, requireAttr(def, 'name'));
  const value = a.value ?? def.element.text.trim();
  const phase = oneOf(def, 'phase', a.phase, ['request', 'response'] as const, 'request');
  const override = parsed(def, () => parseBool(a.override, true));
  if (phase === 'request') {
    return {
      type: 'set-header',
      onRequest(ctx) {
        if (override || ctx.headers[name] === undefined) ctx.headers[name] = expandTemplate(value, ctx);
      },
    };
  }
  return {
    type: 'set-header',
    onResponse(ctx, res) {
      if (override || res.headers[name] === undefined) res.headers[name] = expandTemplate(value, ctx);
    },
  };
};

/** <remove-header name="x-forwarded-for, cookie" phase="request|response"/> */
export const removeHeader: MiddlewareFactory = (def) => {
  const a = attrs(def, ['name', 'phase']);
  const names = splitList(requireAttr(def, 'name')).map((n) => headerName(def, n));
  const phase = oneOf(def, 'phase', a.phase, ['request', 'response'] as const, 'request');
  if (phase === 'request') {
    return {
      type: 'remove-header',
      onRequest(ctx) {
        for (const n of names) delete ctx.headers[n];
      },
    };
  }
  return {
    type: 'remove-header',
    onResponse(_ctx, res) {
      for (const n of names) delete res.headers[n];
    },
  };
};

/** <rewrite-path from="^/v1/(.*)$" to="/api/$1"/> — rewrites the path appended to the upstream URL. */
export const rewritePath: MiddlewareFactory = (def) => {
  attrs(def, ['from', 'to']);
  const pattern = parsed(def, () => new RegExp(requireAttr(def, 'from')));
  const to = def.attrs.to ?? '';
  return {
    type: 'rewrite-path',
    onRequest(ctx) {
      let next = ctx.upstreamPath.replace(pattern, to);
      if (next && !next.startsWith('/')) next = `/${next}`;
      ctx.upstreamPath = next;
    },
  };
};

/** <timeout value="30s"/> — upstream inactivity timeout for this route. */
export const timeout: MiddlewareFactory = (def) => {
  attrs(def, ['value']);
  const ms = parsed(def, () => parseDuration(requireAttr(def, 'value')));
  return {
    type: 'timeout',
    onRequest(ctx) {
      ctx.timeoutMs = ms;
    },
  };
};

function jsonObjectBody(ctx: Context): Record<string, any> | undefined {
  const body = ctx.getJson();
  return body && typeof body === 'object' && !Array.isArray(body) ? body : undefined;
}

/** <json-set path="temperature" value="0.2" type="number" mode="set|default"/> */
export const jsonSet: MiddlewareFactory = (def) => {
  const a = attrs(def, ['path', 'value', 'type', 'mode']);
  const path = requireAttr(def, 'path');
  const type = oneOf(def, 'type', a.type, ['string', 'number', 'boolean', 'json', 'null'] as const, 'string');
  const mode = oneOf(def, 'mode', a.mode, ['set', 'default'] as const, 'set');
  const raw = a.value ?? def.element.text.trim();
  const value = parsed(def, (): unknown => {
    switch (type) {
      case 'number': {
        const n = Number(raw);
        if (raw.trim() === '' || !Number.isFinite(n)) throw new Error(`"${raw}" is not a number`);
        return n;
      }
      case 'boolean':
        return parseBool(raw, false);
      case 'json':
        return JSON.parse(raw);
      case 'null':
        return null;
      default:
        return raw;
    }
  });
  parsed(def, () => setJsonPath({}, path, null)); // validate the path once
  return {
    type: 'json-set',
    onRequest(ctx) {
      const body = jsonObjectBody(ctx);
      if (!body) return;
      if (mode === 'default' && getJsonPath(body, path) !== undefined) return;
      setJsonPath(body, path, structuredClone(value));
      ctx.touchJson();
    },
  };
};

/** <max-tokens limit="4096" default="1024"/> — caps (and optionally defaults) the completion length. */
export const maxTokens: MiddlewareFactory = (def) => {
  const a = attrs(def, ['limit', 'default', 'fields', 'default-field']);
  const limit = parsed(def, () => parseIntStrict(requireAttr(def, 'limit'), 'limit', 1));
  const fallback = a.default ? parsed(def, () => parseIntStrict(a.default!, 'default', 1)) : undefined;
  if (fallback !== undefined && fallback > limit) throw new ConfigError('<max-tokens> default cannot exceed limit', def.element);
  const fields = splitList(a.fields ?? 'max_tokens,max_completion_tokens,max_output_tokens');
  const defaultField = a['default-field'] ?? 'max_tokens';
  return {
    type: 'max-tokens',
    onRequest(ctx) {
      const body = jsonObjectBody(ctx);
      if (!body) return;
      let present = false;
      for (const f of fields) {
        if (body[f] === undefined) continue;
        present = true;
        if (typeof body[f] !== 'number' || body[f] > limit) {
          body[f] = limit;
          ctx.touchJson();
        }
      }
      if (!present && fallback !== undefined) {
        body[defaultField] = fallback;
        ctx.touchJson();
      }
    },
  };
};

/** <model-filter allow="gpt-4o*, claude-*" deny="*-preview" require="false"/> */
export const modelFilter: MiddlewareFactory = (def) => {
  const a = attrs(def, ['allow', 'deny', 'require']);
  const allow = splitList(a.allow).map(globToRegExp);
  const deny = splitList(a.deny).map(globToRegExp);
  const require = parsed(def, () => parseBool(a.require, false));
  if (!allow.length && !deny.length && !require) {
    throw new ConfigError('<model-filter> needs allow, deny or require="true"', def.element);
  }
  return {
    type: 'model-filter',
    onRequest(ctx) {
      const model = ctx.getJson()?.model;
      if (typeof model !== 'string' || model === '') {
        if (require) return errorReply(400, 'model_required', 'A "model" field is required');
        return;
      }
      if (matchesAny(model, deny) || (allow.length > 0 && !matchesAny(model, allow))) {
        return errorReply(403, 'model_not_allowed', `Model "${model}" is not allowed on this route`);
      }
    },
  };
};

/**
 * <system-prompt mode="prepend|append|replace" format="auto|openai|anthropic">Be concise.</system-prompt>
 * Injects a system instruction into chat requests.
 */
export const systemPrompt: MiddlewareFactory = (def) => {
  const a = attrs(def, ['text', 'mode', 'format']);
  const text = (a.text ?? def.element.text).trim();
  if (!text) throw new ConfigError('<system-prompt> needs text (attribute or element content)', def.element);
  const mode = oneOf(def, 'mode', a.mode, ['prepend', 'append', 'replace'] as const, 'prepend');
  const format = oneOf(def, 'format', a.format, ['auto', 'openai', 'anthropic'] as const, 'auto');
  return {
    type: 'system-prompt',
    onRequest(ctx) {
      const body = jsonObjectBody(ctx);
      if (!body) return;
      const anthropic =
        format === 'anthropic' || (format === 'auto' && (body.system !== undefined || ctx.headers['anthropic-version'] !== undefined));
      if (anthropic) {
        if (!Array.isArray(body.messages) && format === 'auto') return;
        const existing = body.system;
        if (mode === 'replace' || existing === undefined || existing === '') body.system = text;
        else if (Array.isArray(existing)) {
          if (mode === 'prepend') existing.unshift({ type: 'text', text });
          else existing.push({ type: 'text', text });
        } else body.system = mode === 'prepend' ? `${text}\n\n${existing}` : `${existing}\n\n${text}`;
        ctx.touchJson();
        return;
      }
      if (!Array.isArray(body.messages)) return;
      const messages: any[] = body.messages;
      const isSystem = (m: any) => m && (m.role === 'system' || m.role === 'developer');
      const message = { role: 'system', content: text };
      if (mode === 'replace') body.messages = [message, ...messages.filter((m) => !isSystem(m))];
      else if (mode === 'prepend') messages.unshift(message);
      else {
        let last = -1;
        messages.forEach((m, i) => {
          if (isSystem(m)) last = i;
        });
        messages.splice(last + 1, 0, message);
      }
      ctx.touchJson();
    },
  };
};

function luhn(digits: string): boolean {
  let sum = 0;
  let double = false;
  for (let i = digits.length - 1; i >= 0; i--) {
    let d = digits.charCodeAt(i) - 48;
    if (double) {
      d *= 2;
      if (d > 9) d -= 9;
    }
    sum += d;
    double = !double;
  }
  return sum % 10 === 0;
}

interface RedactPattern {
  name: string;
  regex: RegExp;
  validate?: (match: string) => boolean;
}

export const REDACT_PATTERNS: Record<string, RedactPattern> = {
  email: { name: 'email', regex: /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g },
  'credit-card': {
    name: 'credit-card',
    regex: /\b(?:\d[ -]?){12,18}\d\b/g,
    validate: (m) => luhn(m.replace(/\D/g, '')),
  },
  ssn: { name: 'ssn', regex: /\b\d{3}-\d{2}-\d{4}\b/g },
  phone: { name: 'phone', regex: /(?<![\w+])(?:\+\d{1,3}[\s.-]?)?(?:\(\d{3}\)|\d{3})[\s.-]?\d{3}[\s.-]?\d{4}(?!\w)/g },
  ipv4: { name: 'ipv4', regex: /\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b/g },
  'api-key': { name: 'api-key', regex: /\b(?:sk|pk|rk)-[A-Za-z0-9_-]{16,}/g },
  'aws-access-key': { name: 'aws-access-key', regex: /\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g },
  jwt: { name: 'jwt', regex: /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g },
};

const DEFAULT_REDACT = ['email', 'credit-card', 'ssn', 'api-key', 'aws-access-key'];

/**
 * <redact patterns="email,credit-card" action="replace|block" replacement="[REDACTED:{name}]">
 *   <pattern name="employee-id">EMP-\d{6}</pattern>
 * </redact>
 * Scrubs sensitive data from request bodies before they reach the model.
 */
export const redact: MiddlewareFactory = (def) => {
  const a = attrs(def, ['patterns', 'action', 'replacement']);
  const action = oneOf(def, 'action', a.action, ['replace', 'block'] as const, 'replace');
  const replacement = a.replacement ?? '[REDACTED:{name}]';
  const patterns: RedactPattern[] = [];
  const custom = def.element.children;
  const names = a.patterns !== undefined ? splitList(a.patterns) : custom.length ? [] : DEFAULT_REDACT;
  for (const n of names) {
    const p = REDACT_PATTERNS[n];
    if (!p) throw new ConfigError(`<redact> unknown pattern "${n}" (built-in: ${Object.keys(REDACT_PATTERNS).join(', ')})`, def.element);
    patterns.push(p);
  }
  for (const child of custom) {
    if (child.name !== 'pattern') throw new ConfigError(`Unknown element <${child.name}> in <redact> (expected <pattern>)`, child);
    checkAttrs(child, ['name', 'flags']);
    const name = child.attrs.name ?? 'custom';
    const source = child.text.trim();
    let regex: RegExp;
    try {
      regex = new RegExp(source, `${(child.attrs.flags ?? '').replace(/g/g, '')}g`);
    } catch (err) {
      throw new ConfigError(`<pattern> invalid regular expression: ${(err as Error).message}`, child);
    }
    if (regex.test('')) throw new ConfigError('<pattern> must not match the empty string', child);
    patterns.push({ name, regex });
  }
  if (!patterns.length) throw new ConfigError('<redact> needs at least one pattern', def.element);

  const scrub = (input: string, found: string[]): string => {
    let out = input;
    for (const p of patterns) {
      p.regex.lastIndex = 0;
      out = out.replace(p.regex, (m) => {
        if (p.validate && !p.validate(m)) return m;
        found.push(p.name);
        return replacement.replace(/\{name\}/g, p.name);
      });
    }
    return out;
  };
  const walk = (value: unknown, found: string[], key?: string): unknown => {
    if (typeof value === 'string') return key === 'model' ? value : scrub(value, found);
    if (Array.isArray(value)) return value.map((v) => walk(v, found));
    if (value && typeof value === 'object') {
      const out: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(value)) out[k] = walk(v, found, k);
      return out;
    }
    return value;
  };

  return {
    type: 'redact',
    onRequest(ctx) {
      if (!ctx.body.length) return;
      const found: string[] = [];
      const json = ctx.getJson();
      let next: unknown;
      let text: string | undefined;
      if (json !== undefined) next = walk(json, found);
      else if (/^text\//i.test(ctx.headers['content-type'] ?? '')) text = scrub(ctx.body.toString('utf8'), found);
      else return;
      if (!found.length) return;
      if (action === 'block') {
        return errorReply(400, 'sensitive_content', `Request contains sensitive data (${[...new Set(found)].join(', ')})`);
      }
      if (text !== undefined) ctx.body = Buffer.from(text);
      else ctx.setJson(next);
      ctx.state.set('redact.count', found.length);
    },
    onResponse(ctx, res) {
      const count = ctx.state.get('redact.count');
      if (count) res.headers['x-moxy-redactions'] = String(count);
    },
  };
};
