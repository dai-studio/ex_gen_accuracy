#!/usr/bin/env node
import { existsSync } from 'node:fs';
import { parseArgs } from 'node:util';
import { generateApiKey, hashApiKey, hashPassword, RevocationList } from './auth.ts';
import { ConfigError, loadConfigFile } from './config.ts';
import { Gateway } from './gateway.ts';
import { buildRuntime } from './runtime.ts';
import { Logger } from './logger.ts';
import { VERSION } from './version.ts';

const USAGE = `moxy ${VERSION} — gateway for AI model endpoints

Usage:
  moxy [serve] [-c moxy.xml] [--host HOST] [--port PORT]   start the gateway
  moxy validate [-c moxy.xml]                             check a configuration file
  moxy hash-password [PASSWORD]                           print a password-hash for <user> (reads stdin if omitted)
  moxy gen-key                                            generate an API key and its key-hash
  moxy hash-key KEY                                       print the key-hash for an existing API key
  moxy version

The config path defaults to $MOXY_CONFIG, then ./moxy.xml.
Send SIGHUP (or POST /admin/api/reload) to reload the configuration.`;

function configPath(explicit: string | undefined): string {
  return explicit ?? process.env.MOXY_CONFIG ?? 'moxy.xml';
}

async function readStdin(): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) chunks.push(chunk as Buffer);
  return Buffer.concat(chunks).toString('utf8').replace(/\r?\n$/, '');
}

async function main(argv: string[]): Promise<number> {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      config: { type: 'string', short: 'c' },
      host: { type: 'string' },
      port: { type: 'string', short: 'p' },
      help: { type: 'boolean', short: 'h' },
    },
  });
  const command = positionals[0] ?? 'serve';
  if (values.help || command === 'help') {
    console.log(USAGE);
    return 0;
  }

  switch (command) {
    case 'version':
      console.log(VERSION);
      return 0;

    case 'hash-password': {
      const password = positionals[1] ?? (await readStdin());
      if (!password) {
        console.error('error: empty password');
        return 1;
      }
      console.log(await hashPassword(password));
      return 0;
    }

    case 'gen-key': {
      const key = generateApiKey();
      console.log(`key:      ${key}`);
      console.log(`key-hash: sha256:${hashApiKey(key)}`);
      return 0;
    }

    case 'hash-key': {
      if (!positionals[1]) {
        console.error('usage: moxy hash-key KEY');
        return 1;
      }
      console.log(`sha256:${hashApiKey(positionals[1])}`);
      return 0;
    }

    case 'validate': {
      const path = configPath(values.config);
      const config = loadConfigFile(path);
      // Building the runtime instantiates every middleware, which validates their attributes.
      buildRuntime(config, { logger: new Logger('error'), secret: 'x'.repeat(32), revocations: new RevocationList() });
      const models = config.routes.reduce((n, r) => n + (r.models?.models.length ?? 0), 0);
      console.log(`${path}: OK (${config.routes.length} routes, ${models} models, ${config.auth.users.length} users, ${config.auth.apiKeys.length} api keys)`);
      return 0;
    }

    case 'serve': {
      const path = configPath(values.config);
      if (!existsSync(path)) {
        console.error(`error: config file not found: ${path} (pass -c or set MOXY_CONFIG; see examples/moxy.xml)`);
        return 1;
      }
      const config = loadConfigFile(path);
      const gateway = new Gateway({ config });
      const port = values.port !== undefined ? Number(values.port) : undefined;
      if (port !== undefined && (!Number.isInteger(port) || port < 0 || port > 65535)) {
        console.error(`error: invalid port "${values.port}"`);
        return 1;
      }
      await gateway.listen(port, values.host);
      process.on('SIGHUP', () => gateway.reload());
      let stopping = false;
      const stop = (signal: string) => {
        if (stopping) process.exit(1);
        stopping = true;
        gateway.logger.info('shutting down', { signal });
        void gateway.close().then(() => process.exit(0));
      };
      process.on('SIGINT', () => stop('SIGINT'));
      process.on('SIGTERM', () => stop('SIGTERM'));
      return -1; // keep running
    }

    default:
      console.error(`Unknown command "${command}"\n\n${USAGE}`);
      return 1;
  }
}

main(process.argv.slice(2)).then(
  (code) => {
    if (code >= 0) process.exitCode = code;
  },
  (err: unknown) => {
    if (err instanceof ConfigError) console.error(`config error: ${err.message}`);
    else console.error(`error: ${(err as Error).message ?? err}`);
    process.exitCode = 1;
  },
);
