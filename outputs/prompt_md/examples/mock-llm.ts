// A tiny OpenAI-compatible model server for trying Moxy without real API keys.
//   node examples/mock-llm.ts [port]
// Implements GET /v1/models, POST /v1/chat/completions (with and without
// streaming) and POST /v1/embeddings. Replies echo the last user message.
import http from 'node:http';

const port = Number(process.argv[2] ?? 9001);

function lastUserMessage(body: any): string {
  const messages: any[] = Array.isArray(body?.messages) ? body.messages : [];
  const last = [...messages].reverse().find((m) => m?.role === 'user');
  if (!last) return '';
  return typeof last.content === 'string' ? last.content : JSON.stringify(last.content);
}

const server = http.createServer((req, res) => {
  const chunks: Buffer[] = [];
  req.on('data', (c: Buffer) => chunks.push(c));
  req.on('end', () => {
    let body: any = {};
    try {
      body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : {};
    } catch {
      res.writeHead(400, { 'content-type': 'application/json' }).end('{"error":{"message":"invalid json"}}');
      return;
    }
    const path = (req.url ?? '/').split('?')[0];
    if (req.method === 'GET' && path === '/v1/models') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ object: 'list', data: [{ id: 'mock-small', object: 'model' }, { id: 'mock-large', object: 'model' }] }));
      return;
    }
    if (req.method === 'POST' && path === '/v1/embeddings') {
      const input = Array.isArray(body.input) ? body.input : [body.input ?? ''];
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(
        JSON.stringify({
          object: 'list',
          model: body.model,
          data: input.map((text: string, index: number) => ({ object: 'embedding', index, embedding: [String(text).length, 0.5, -0.5] })),
          usage: { prompt_tokens: input.length * 3, total_tokens: input.length * 3 },
        }),
      );
      return;
    }
    if (req.method === 'POST' && path === '/v1/chat/completions') {
      const system = (body.messages ?? []).filter((m: any) => m.role === 'system').map((m: any) => m.content);
      const reply = `You said: "${lastUserMessage(body)}"${system.length ? ` (system: ${system.join(' | ')})` : ''} [model=${body.model}]`;
      const usage = { prompt_tokens: 12, completion_tokens: reply.split(/\s+/).length, total_tokens: 12 + reply.split(/\s+/).length };
      if (body.stream) {
        res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
        const words = reply.split(/(?<= )/);
        let i = 0;
        const timer = setInterval(() => {
          if (i < words.length) {
            res.write(`data: ${JSON.stringify({ id: 'mock', object: 'chat.completion.chunk', model: body.model, choices: [{ index: 0, delta: { content: words[i++] } }] })}\n\n`);
            return;
          }
          clearInterval(timer);
          if (body.stream_options?.include_usage) {
            res.write(`data: ${JSON.stringify({ id: 'mock', object: 'chat.completion.chunk', model: body.model, choices: [], usage })}\n\n`);
          }
          res.end('data: [DONE]\n\n');
        }, 40);
        res.on('close', () => clearInterval(timer));
        return;
      }
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(
        JSON.stringify({
          id: 'mock',
          object: 'chat.completion',
          model: body.model,
          choices: [{ index: 0, message: { role: 'assistant', content: reply }, finish_reason: 'stop' }],
          usage,
        }),
      );
      return;
    }
    res.writeHead(404, { 'content-type': 'application/json' }).end('{"error":{"message":"not found"}}');
  });
});

server.listen(port, '127.0.0.1', () => console.log(`mock LLM listening on http://127.0.0.1:${port}`));
