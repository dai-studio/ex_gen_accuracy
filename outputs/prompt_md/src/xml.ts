// A small, dependency-free, non-validating XML parser. It covers what a
// configuration file needs: elements, attributes, text, CDATA, comments,
// processing instructions and the predefined/numeric entities. DOCTYPE
// declarations are rejected outright so entity-expansion attacks are impossible.

export interface XmlElement {
  name: string;
  attrs: Record<string, string>;
  children: XmlElement[];
  /** Concatenated direct text content (including CDATA), not trimmed. */
  text: string;
  line: number;
  column: number;
}

export class XmlError extends Error {
  readonly line: number;
  readonly column: number;

  constructor(message: string, line: number, column: number) {
    super(`${message} (line ${line}, column ${column})`);
    this.name = 'XmlError';
    this.line = line;
    this.column = column;
  }
}

const NAME_START = /[A-Za-z_:]/;
const NAME_CHAR = /[A-Za-z0-9_:.\-]/;
const WS = /[ \t\r\n]/;

const ENTITIES: Record<string, string> = {
  lt: '<',
  gt: '>',
  amp: '&',
  quot: '"',
  apos: "'",
};

export function parseXml(source: string): XmlElement {
  return new Parser(source).parseDocument();
}

class Parser {
  private readonly src: string;
  private readonly lineStarts: number[] = [0];
  private i = 0;

  constructor(source: string) {
    this.src = source.charCodeAt(0) === 0xfeff ? source.slice(1) : source;
    for (let k = 0; k < this.src.length; k++) {
      if (this.src.charCodeAt(k) === 10) this.lineStarts.push(k + 1);
    }
  }

  parseDocument(): XmlElement {
    this.skipMisc();
    if (this.src[this.i] !== '<') this.fail('Expected a root element');
    const root = this.parseElement();
    this.skipMisc();
    if (this.i < this.src.length) this.fail('Unexpected content after the root element');
    return root;
  }

  private position(at: number): { line: number; column: number } {
    let lo = 0;
    let hi = this.lineStarts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (this.lineStarts[mid]! <= at) lo = mid;
      else hi = mid - 1;
    }
    return { line: lo + 1, column: at - this.lineStarts[lo]! + 1 };
  }

  private fail(message: string, at = this.i): never {
    const { line, column } = this.position(at);
    throw new XmlError(message, line, column);
  }

  private startsWith(s: string): boolean {
    return this.src.startsWith(s, this.i);
  }

  private skipWs(): boolean {
    const start = this.i;
    while (this.i < this.src.length && WS.test(this.src[this.i]!)) this.i++;
    return this.i > start;
  }

  private skipUntil(terminator: string, what: string): void {
    const end = this.src.indexOf(terminator, this.i);
    if (end < 0) this.fail(`Unterminated ${what}`);
    this.i = end + terminator.length;
  }

  private skipMisc(): void {
    for (;;) {
      this.skipWs();
      if (this.startsWith('<?')) this.skipUntil('?>', 'processing instruction');
      else if (this.startsWith('<!--')) this.skipUntil('-->', 'comment');
      else if (this.startsWith('<!')) this.fail('DOCTYPE and other declarations are not supported');
      else return;
    }
  }

  private readName(): string {
    const start = this.i;
    if (!NAME_START.test(this.src[this.i] ?? '')) this.fail('Expected a name');
    this.i++;
    while (this.i < this.src.length && NAME_CHAR.test(this.src[this.i]!)) this.i++;
    return this.src.slice(start, this.i);
  }

  private decode(raw: string, offset: number): string {
    if (!raw.includes('&')) return raw;
    return raw.replace(/&(#x[0-9a-fA-F]+|#[0-9]+|[A-Za-z]+);|&/g, (match, ref: string | undefined, index: number) => {
      if (!ref) this.fail('Unescaped "&" (use &amp;)', offset + index);
      let code: number | undefined;
      if (ref.startsWith('#x')) code = parseInt(ref.slice(2), 16);
      else if (ref.startsWith('#')) code = parseInt(ref.slice(1), 10);
      else if (ref in ENTITIES) return ENTITIES[ref]!;
      else this.fail(`Unknown entity "${match}"`, offset + index);
      if (code === undefined || code > 0x10ffff) this.fail(`Invalid character reference "${match}"`, offset + index);
      return String.fromCodePoint(code);
    });
  }

  private parseElement(): XmlElement {
    const start = this.i;
    this.i++; // '<'
    const name = this.readName();
    const { line, column } = this.position(start);
    const el: XmlElement = { name, attrs: {}, children: [], text: '', line, column };

    for (;;) {
      const hadWs = this.skipWs();
      if (this.startsWith('/>')) {
        this.i += 2;
        return el;
      }
      if (this.src[this.i] === '>') {
        this.i++;
        break;
      }
      if (this.i >= this.src.length) this.fail(`Unterminated start tag <${name}>`, start);
      if (!hadWs) this.fail('Expected whitespace before attribute');
      const attrStart = this.i;
      const attrName = this.readName();
      if (attrName === '__proto__') this.fail('Invalid attribute name', attrStart);
      if (Object.hasOwn(el.attrs, attrName)) this.fail(`Duplicate attribute "${attrName}"`, attrStart);
      this.skipWs();
      if (this.src[this.i] !== '=') this.fail(`Expected "=" after attribute "${attrName}"`);
      this.i++;
      this.skipWs();
      const quote = this.src[this.i];
      if (quote !== '"' && quote !== "'") this.fail('Expected a quoted attribute value');
      const valueStart = this.i + 1;
      const valueEnd = this.src.indexOf(quote, valueStart);
      if (valueEnd < 0) this.fail('Unterminated attribute value');
      const raw = this.src.slice(valueStart, valueEnd);
      const lt = raw.indexOf('<');
      if (lt >= 0) this.fail('"<" is not allowed in attribute values', valueStart + lt);
      el.attrs[attrName] = this.decode(raw, valueStart);
      this.i = valueEnd + 1;
    }

    for (;;) {
      if (this.i >= this.src.length) this.fail(`Unclosed element <${name}>`, start);
      if (this.startsWith('</')) {
        this.i += 2;
        const closeAt = this.i;
        const close = this.readName();
        if (close !== name) this.fail(`Mismatched closing tag </${close}>, expected </${name}>`, closeAt);
        this.skipWs();
        if (this.src[this.i] !== '>') this.fail('Expected ">"');
        this.i++;
        return el;
      }
      if (this.startsWith('<!--')) {
        this.skipUntil('-->', 'comment');
      } else if (this.startsWith('<![CDATA[')) {
        const end = this.src.indexOf(']]>', this.i + 9);
        if (end < 0) this.fail('Unterminated CDATA section');
        el.text += this.src.slice(this.i + 9, end);
        this.i = end + 3;
      } else if (this.startsWith('<?')) {
        this.skipUntil('?>', 'processing instruction');
      } else if (this.startsWith('<!')) {
        this.fail('Declarations are not allowed inside elements');
      } else if (this.src[this.i] === '<') {
        el.children.push(this.parseElement());
      } else {
        let end = this.src.indexOf('<', this.i);
        if (end < 0) end = this.src.length;
        el.text += this.decode(this.src.slice(this.i, end), this.i);
        this.i = end;
      }
    }
  }
}
