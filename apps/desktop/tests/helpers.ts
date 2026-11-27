// Test helpers: scratch folders and a scripted stand-in for `koda serve`.
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { afterEach } from 'vitest';
import type { RpcMessage } from '../src/main/koda-client';

const servers: net.Server[] = [];
const folders: string[] = [];

afterEach(() => {
  for (const server of servers.splice(0)) server.close();
  for (const folder of folders.splice(0)) rmSync(folder, { recursive: true, force: true });
});

/** A scratch folder, removed after the test. Short, for socket paths (104 bytes on macOS). */
export function scratch(): string {
  const folder = mkdtempSync(path.join(os.tmpdir(), 'kd-'));
  folders.push(folder);
  return folder;
}

/** An executable shell script at `name` in `folder`. */
export function script(folder: string, name: string, body: string): string {
  const file = path.join(folder, name);
  writeFileSync(file, `#!/bin/sh\n${body}\n`);
  chmodSync(file, 0o755);
  return file;
}

export interface Scripted {
  socket: string;
  /** Send a line to every connected client. */
  broadcast(message: object): void;
  /** Close every connection and stop listening. */
  stop(): void;
}

/** A server on a scratch socket that answers each request with `reply`. */
export async function scripted(
  reply: (message: RpcMessage, write: (m: object | string) => void, hangUp: () => void) => void,
): Promise<Scripted> {
  const socket = path.join(scratch(), 's.sock');
  const connections = new Set<net.Socket>();
  const server = net.createServer((connection) => {
    connections.add(connection);
    connection.on('close', () => connections.delete(connection));
    let buffer = '';
    const write = (m: object | string) =>
      connection.write((typeof m === 'string' ? m : JSON.stringify(m)) + '\n');
    connection.setEncoding('utf8');
    connection.on('data', (chunk: string) => {
      buffer += chunk;
      let end;
      while ((end = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, end);
        buffer = buffer.slice(end + 1);
        reply(JSON.parse(line) as RpcMessage, write, () => connection.destroy());
      }
    });
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(socket, resolve));
  return {
    socket,
    broadcast: (message) => {
      for (const c of connections) c.write(JSON.stringify(message) + '\n');
    },
    stop: () => {
      for (const c of connections) c.destroy();
      server.close();
    },
  };
}

/** A stand-in for `koda serve` that answers `initialize` with `version`. */
export function kodaServe(version: string) {
  return scripted((m, write) => {
    if (m.method === 'initialize') {
      write({ jsonrpc: '2.0', id: m.id, result: { serverInfo: { name: 'koda', version } } });
    } else if (m.id !== undefined) {
      write({ jsonrpc: '2.0', id: m.id, result: {} });
    }
  });
}
