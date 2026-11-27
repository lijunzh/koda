// A client of `koda serve` over its Unix socket: JSON-RPC 2.0, one JSON
// object per line each way, `initialize` first, as `docs/src/serve.md`
// describes. The main process owns it; the page reaches it only through the
// checked relay (`relay.ts`).
import { EventEmitter } from 'node:events';
import net from 'node:net';

export type RpcId = number | string;

export interface RpcMessage {
  jsonrpc: '2.0';
  id?: RpcId;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

/** An error the server answered a request with. */
export class RpcError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly data?: unknown,
  ) {
    super(message);
  }
}

/**
 * Emits `messages` with each batch of notifications and server requests, in
 * the order the server sent them, and `close` when the connection ends.
 */
export class KodaClient extends EventEmitter {
  private socket?: net.Socket;
  private buffer = '';
  private nextId = 1;
  private readonly pending = new Map<
    RpcId,
    { resolve: (value: unknown) => void; reject: (error: Error) => void }
  >();
  /** Requests the server sent that no client has answered yet. Only these may be answered. */
  readonly openServerRequests = new Set<RpcId>();

  /** Connect to the socket at `path`. Rejects if nothing listens there. */
  connect(path: string): Promise<void> {
    return new Promise((resolve, reject) => {
      const socket = net.createConnection({ path });
      socket.setEncoding('utf8'); // decodes UTF-8 across chunk boundaries
      socket.once('connect', () => {
        this.socket = socket;
        socket.off('error', reject);
        socket.on('error', () => socket.destroy());
        resolve();
      });
      socket.once('error', reject);
      socket.on('data', (chunk: string) => this.read(chunk));
      socket.on('close', () => {
        for (const waiting of this.pending.values()) {
          waiting.reject(new Error('the connection to koda serve closed'));
        }
        this.pending.clear();
        this.openServerRequests.clear();
        this.socket = undefined;
        this.emit('close');
      });
    });
  }

  private read(chunk: string) {
    this.buffer += chunk;
    const batch: RpcMessage[] = [];
    let start = 0;
    for (;;) {
      const end = this.buffer.indexOf('\n', start);
      if (end < 0) break;
      const line = this.buffer.slice(start, end);
      start = end + 1;
      if (!line.trim()) continue;
      let message: RpcMessage;
      try {
        message = JSON.parse(line) as RpcMessage;
      } catch {
        continue; // not JSON: skipped, as Koda's Rust client skips it
      }
      if (message.method === undefined && message.id !== undefined) {
        const waiting = this.pending.get(message.id);
        if (!waiting) continue;
        this.pending.delete(message.id);
        if (message.error) {
          waiting.reject(new RpcError(message.error.code, message.error.message, message.error.data));
        } else {
          waiting.resolve(message.result);
        }
        continue;
      }
      if (message.method !== undefined && message.id !== undefined) {
        this.openServerRequests.add(message.id);
      }
      if (message.method === 'serverRequest/resolved') {
        const answered = (message.params as { requestId?: RpcId } | undefined)?.requestId;
        if (answered !== undefined) this.openServerRequests.delete(answered);
      }
      batch.push(message);
    }
    this.buffer = this.buffer.slice(start);
    if (batch.length) this.emit('messages', batch);
  }

  private write(message: RpcMessage) {
    if (!this.socket) throw new Error('not connected to koda serve');
    this.socket.write(JSON.stringify(message) + '\n');
  }

  /** Send a request and wait for its answer. */
  request<T = unknown>(method: string, params: unknown = {}): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
      try {
        this.write({ jsonrpc: '2.0', id, method, params });
      } catch (error) {
        this.pending.delete(id);
        reject(error as Error);
      }
    });
  }

  /** Answer a request the server sent, such as an approval. */
  respond(id: RpcId, result: unknown) {
    this.openServerRequests.delete(id);
    this.write({ jsonrpc: '2.0', id, result });
  }

  /** Refuse a request the server sent. */
  respondError(id: RpcId, code: number, message: string) {
    this.openServerRequests.delete(id);
    this.write({ jsonrpc: '2.0', id, error: { code, message } });
  }

  get connected(): boolean {
    return this.socket !== undefined;
  }

  close() {
    this.socket?.end();
    this.socket?.destroy();
  }
}
