// A scripted model for the app tests: an OpenAI-compatible server that
// streams the replies a test gives it, one per request, in order.
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { writeFileSync, mkdirSync } from 'node:fs';
import path from 'node:path';

/** A gate a test opens: a reply with `after` starts only once it is open. */
export interface Gate {
  opened: Promise<void>;
  open(): void;
}

export function gate(): Gate {
  let open!: () => void;
  const opened = new Promise<void>((resolve) => (open = resolve));
  return { opened, open };
}

/**
 * One reply: text, streamed in pieces, or a tool call; `delayMs` between
 * pieces. With `after`, the request is taken and the text waits for the gate,
 * so the turn stays running for as long as the test needs it to.
 */
export type Reply =
  | { text: string; pieces?: number; delayMs?: number; after?: Gate }
  | { tool: string; args: object };

export interface Model {
  url: string;
  /** Requests answered so far. */
  calls(): number;
  /** The bodies of the requests, in order. */
  bodies(): string[];
  close(): void;
}

const line = (chunk: object) => `data: ${JSON.stringify(chunk)}\n\n`;

export async function scriptedModel(replies: Reply[]): Promise<Model> {
  let calls = 0;
  const bodies: string[] = [];
  const server = http.createServer((req, res) => {
    if (req.method !== 'POST' || !req.url?.endsWith('/chat/completions')) {
      res.writeHead(404).end();
      return;
    }
    let body = '';
    req.setEncoding('utf8');
    req.on('data', (chunk: string) => (body += chunk));
    req.on('end', async () => {
      bodies.push(body);
      const reply = replies[calls++] ?? { text: 'All done.' };
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if ('tool' in reply) {
        res.write(
          line({
            choices: [
              {
                delta: {
                  tool_calls: [
                    { index: 0, id: `call_${calls}`, type: 'function', function: { name: reply.tool, arguments: JSON.stringify(reply.args) } },
                  ],
                },
              },
            ],
          }),
        );
        res.write(line({ choices: [{ delta: {}, finish_reason: 'tool_calls' }] }));
      } else {
        if (reply.after) {
          // The headers go now, so the server has its answer; the text waits.
          res.flushHeaders();
          await reply.after.opened;
          if (res.destroyed) return;
        }
        const pieces = reply.pieces ?? 1;
        const size = Math.ceil(reply.text.length / pieces);
        for (let i = 0; i < reply.text.length; i += size) {
          if (res.destroyed) return;
          res.write(line({ choices: [{ delta: { content: reply.text.slice(i, i + size) } }] }));
          if (reply.delayMs) await new Promise((r) => setTimeout(r, reply.delayMs));
        }
        res.write(line({ choices: [{ delta: {}, finish_reason: 'stop' }] }));
      }
      res.write(line({ choices: [], usage: { prompt_tokens: 20, completion_tokens: 5 } }));
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${port}/v1`,
    calls: () => calls,
    bodies: () => bodies.slice(),
    close: () => {
      server.close();
      // A reply still waiting at its gate holds its connection: let it go.
      server.closeAllConnections();
    },
  };
}

/** A Koda config under `home` that runs new sessions on `model`. */
export function useModel(home: string, model: Model) {
  mkdirSync(path.join(home, 'koda'), { recursive: true });
  writeFileSync(
    path.join(home, 'koda', 'config.toml'),
    [
      'model_provider = "lm-studio"',
      'model = "stub"',
      'model_context_window = 100000',
      '',
      '[model_providers.lm-studio]',
      `base_url = "${model.url}"`,
      '',
    ].join('\n'),
  );
}
