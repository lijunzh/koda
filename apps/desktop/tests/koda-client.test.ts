// The socket client against a scripted server on a scratch socket.
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { KodaClient, RpcError, type RpcMessage } from '../src/main/koda-client';
import { scratch, scripted } from './helpers';

describe('KodaClient', () => {
  it('matches each answer to its request, whatever the order', async () => {
    const { socket } = await scripted((m, write) => {
      const answer = { jsonrpc: '2.0', id: m.id, result: { method: m.method } };
      // The first request is answered last.
      if (m.method === 'slow') setTimeout(() => write(answer), 20);
      else write(answer);
    });
    const client = new KodaClient();
    await client.connect(socket);
    const [slow, fast] = await Promise.all([client.request('slow'), client.request('fast')]);
    expect(slow).toEqual({ method: 'slow' });
    expect(fast).toEqual({ method: 'fast' });
    client.close();
  });

  it('rejects with the error the server answered', async () => {
    const { socket } = await scripted((m, write) =>
      write({ jsonrpc: '2.0', id: m.id, error: { code: -32601, message: 'no such method' } }),
    );
    const client = new KodaClient();
    await client.connect(socket);
    const failure = await client.request('nope').catch((e: unknown) => e);
    expect(failure).toBeInstanceOf(RpcError);
    expect((failure as RpcError).code).toBe(-32601);
    client.close();
  });

  it('passes on notifications and server requests in order, and only an open request may be answered', async () => {
    let answered: RpcMessage | undefined;
    const { socket } = await scripted((m, write) => {
      if (m.method === 'go') {
        // Two messages and a line that isn't JSON, in one write.
        write(
          [
            JSON.stringify({ jsonrpc: '2.0', method: 'turn/started', params: { n: 1 } }),
            'not json',
            JSON.stringify({ jsonrpc: '2.0', id: 'r1', method: 'item/toolCall/requestApproval', params: {} }),
          ].join('\n'),
        );
        write({ jsonrpc: '2.0', id: m.id, result: {} });
      } else if (m.id === 'r1' && m.method === undefined) {
        answered = m;
      }
    });
    const client = new KodaClient();
    const batches: RpcMessage[][] = [];
    client.on('messages', (batch: RpcMessage[]) => batches.push(batch));
    await client.connect(socket);
    await client.request('go');
    const methods = batches.flat().map((m) => m.method);
    expect(methods).toEqual(['turn/started', 'item/toolCall/requestApproval']);
    expect(client.openServerRequests.has('r1')).toBe(true);
    client.respond('r1', { decision: { type: 'approve' } });
    expect(client.openServerRequests.has('r1')).toBe(false);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(answered?.result).toEqual({ decision: { type: 'approve' } });
    client.close();
  });

  it('forgets a request another client answered', async () => {
    const { socket } = await scripted((m, write) => {
      write({ jsonrpc: '2.0', id: 'r2', method: 'item/toolCall/requestApproval', params: {} });
      write({ jsonrpc: '2.0', method: 'serverRequest/resolved', params: { requestId: 'r2' } });
      write({ jsonrpc: '2.0', id: m.id, result: {} });
    });
    const client = new KodaClient();
    await client.connect(socket);
    await client.request('go');
    expect(client.openServerRequests.has('r2')).toBe(false);
    client.close();
  });

  it('fails waiting requests when the server goes away', async () => {
    // It hangs up instead of answering.
    const { socket } = await scripted((_m, _write, hangUp) => hangUp());
    const client = new KodaClient();
    await client.connect(socket);
    const closed = new Promise((resolve) => client.once('close', resolve));
    await expect(client.request('hang')).rejects.toThrow(/closed/);
    await closed;
    expect(client.connected).toBe(false);
  });

  it('rejects connecting where nothing listens', async () => {
    await expect(new KodaClient().connect(path.join(scratch(), 'none.sock'))).rejects.toThrow();
  });
});
