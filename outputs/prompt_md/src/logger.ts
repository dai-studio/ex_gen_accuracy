import type { LogLevel } from './config.ts';

const ORDER: Record<LogLevel, number> = { debug: 10, info: 20, warn: 30, error: 40, silent: 100 };

export type LogFields = Record<string, unknown>;

export interface LogSink {
  write(line: string): unknown;
}

/** Minimal structured logger writing one line per entry (JSON or text). */
export class Logger {
  level: LogLevel;
  format: 'json' | 'text';
  private readonly sink: LogSink;

  constructor(level: LogLevel = 'info', format: 'json' | 'text' = 'json', sink: LogSink = process.stdout) {
    this.level = level;
    this.format = format;
    this.sink = sink;
  }

  debug(msg: string, fields?: LogFields): void {
    this.log('debug', msg, fields);
  }

  info(msg: string, fields?: LogFields): void {
    this.log('info', msg, fields);
  }

  warn(msg: string, fields?: LogFields): void {
    this.log('warn', msg, fields);
  }

  error(msg: string, fields?: LogFields): void {
    this.log('error', msg, fields);
  }

  log(level: Exclude<LogLevel, 'silent'>, msg: string, fields: LogFields = {}): void {
    if (ORDER[level] < ORDER[this.level]) return;
    const time = new Date().toISOString();
    if (this.format === 'json') {
      this.sink.write(`${JSON.stringify({ time, level, msg, ...fields })}\n`);
      return;
    }
    const extras = Object.entries(fields)
      .filter(([, v]) => v !== undefined)
      .map(([k, v]) => `${k}=${typeof v === 'string' ? v : JSON.stringify(v)}`)
      .join(' ');
    this.sink.write(`${time} ${level.toUpperCase().padEnd(5)} ${msg}${extras ? ` ${extras}` : ''}\n`);
  }
}
