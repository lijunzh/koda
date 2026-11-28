// Typed access to the preload bridge: method names and shapes come from the
// types generated from Koda's protocol schema.
import type { Methods, ServerRequests, Session } from '../protocol/types.gen';
import type { ServerStatus } from '../main/server';

export type { ServerStatus };

export interface RpcMessage {
  jsonrpc: '2.0';
  id?: number | string;
  method?: string;
  params?: unknown;
}

interface KodaBridge {
  platform: string;
  onFullScreen(listener: (full: boolean) => void): () => void;
  status(): Promise<ServerStatus>;
  onStatus(listener: (status: ServerStatus) => void): () => void;
  request(method: string, params: unknown): Promise<unknown>;
  respond(id: number | string, result: unknown): Promise<void>;
  onMessages(listener: (batch: RpcMessage[]) => void): () => void;
  pickFolder(): Promise<string | null>;
  startSession(options: { folder: string; provider?: string; model?: string }): Promise<Session>;
  deleteSession(id: string): Promise<boolean>;
  setMode(id: string, mode: 'auto' | 'plan'): Promise<boolean>;
  setAppPrefs(prefs: { notifications: boolean; keepAwake: boolean }): Promise<void>;
  replaceServer(turns: 'wait' | 'stop'): Promise<void>;
  retry(): Promise<void>;
  relaunch(): Promise<void>;
}

declare global {
  interface Window {
    koda: KodaBridge;
  }
}

/** Call a method of Koda's protocol, typed by its name. */
export function call<M extends keyof Methods>(
  method: M,
  params: Methods[M]['params'],
): Promise<Methods[M]['result']> {
  return window.koda.request(method, params) as Promise<Methods[M]['result']>;
}

/** Answer a request the server sent, typed by its method. */
export function answer<M extends keyof ServerRequests>(
  _method: M,
  id: number | string,
  result: ServerRequests[M]['result'],
): Promise<void> {
  return window.koda.respond(id, result);
}
