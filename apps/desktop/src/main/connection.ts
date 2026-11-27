// The app's connection to Koda's background server, through its life:
// started or joined with `koda serve --start`, as `koda` does at start-up;
// replaced after an upgrade by the same rule (an older server with turns
// running waits for the user's choice); given up when Koda was updated past
// this app; and, if the server goes away, tried again on Codex's schedule,
// starting a new one when none answers, as the TUI reconnects.
import { EventEmitter } from 'node:events';
import { KodaClient, type RpcId, type RpcMessage } from './koda-client';
import {
  compareVersions,
  ready,
  type RunningTurns,
  type ServerStatus,
  type StartAnswer,
  startServer,
} from './server';

/** Waits before each reconnect try, in ms: at once, then 1, 2 and 4 seconds, then every 8 (Codex's TUI). */
const FIRST_WAITS = [0, 1000, 2000, 4000];
const LATER_WAIT = 8000;
/** How long reconnecting goes on, waits included (Codex's TUI: two minutes). */
const RECONNECT_BUDGET_MS = 120_000;

export interface ConnectionDeps {
  appVersion: string;
  /** The `koda` that starts the server; `null` if none was found. */
  koda: string | null;
  /** The environment a server started from here gets. */
  env: NodeJS.ProcessEnv;
  /** A socket other than Koda's own (tests). */
  socket?: string;
  start?: typeof startServer;
  newClient?: () => KodaClient;
  sleep?: (ms: number) => Promise<void>;
  reconnectBudgetMs?: number;
}

/**
 * Emits `status` (a `ServerStatus`) whenever it changes, and `messages` with
 * each batch of the server's notifications and requests.
 */
export class Connection extends EventEmitter {
  private client?: KodaClient;
  private current: ServerStatus = { state: 'starting' };
  /** Set by `serve/updating` for a newer Koda: this app can't go on with it. */
  private updatedTo?: string;
  private closing = false;
  private readonly start: typeof startServer;
  private readonly newClient: () => KodaClient;
  private readonly sleep: (ms: number) => Promise<void>;

  constructor(private readonly deps: ConnectionDeps) {
    super();
    this.start = deps.start ?? startServer;
    this.newClient = deps.newClient ?? (() => new KodaClient());
    this.sleep = deps.sleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
  }

  get status(): ServerStatus {
    return this.current;
  }

  private set(status: ServerStatus) {
    this.current = status;
    this.emit('status', status);
  }

  /**
   * Make sure a server of the app's version runs, and connect to it.
   * `runningTurns` is the user's answer when an older server has turns running.
   */
  async open(runningTurns?: RunningTurns): Promise<void> {
    if (!this.deps.koda) {
      this.set({ state: 'noKoda' });
      return;
    }
    const busy = this.current.state === 'busy' ? this.current : undefined;
    this.set(runningTurns === 'wait' && busy ? { state: 'waiting', working: busy.working } : { state: 'starting' });
    const answer = await this.start({
      koda: this.deps.koda,
      env: this.deps.env,
      socket: this.deps.socket,
      runningTurns,
    });
    await this.settle(answer);
  }

  /** Act on `koda serve --start`'s answer. Whether it connected. */
  private async settle(answer: StartAnswer): Promise<boolean> {
    const appVersion = this.deps.appVersion;
    if (ready(answer) && answer.socketPath) return this.connect(answer.socketPath);
    switch (answer.status) {
      case 'busy':
        this.set({
          state: 'busy',
          serverVersion: answer.serverVersion ?? '?',
          appVersion,
          working: answer.working ?? 0,
          draining: answer.draining ?? false,
        });
        return false;
      case 'newerServer':
        this.set({ state: 'updated', version: answer.serverVersion ?? '?', appVersion });
        return false;
      default:
        this.set({ state: 'unreachable', error: answer.message ?? `koda serve --start: ${answer.status}` });
        return false;
    }
  }

  private async connect(socket: string): Promise<boolean> {
    const client = this.newClient();
    try {
      await client.connect(socket);
      const init = await client.request<{ serverInfo: { version: string } }>('initialize', {
        clientInfo: { name: 'koda-desktop', title: 'Koda', version: this.deps.appVersion },
      });
      const serverVersion = init.serverInfo.version;
      const match = compareVersions(serverVersion, this.deps.appVersion);
      if (match !== 'same') {
        // Replaced between `koda serve --start` and connecting.
        client.close();
        if (match === 'newer') {
          this.set({ state: 'updated', version: serverVersion, appVersion: this.deps.appVersion });
        } else {
          this.set({ state: 'unreachable', error: `the server at ${socket} runs ${serverVersion}` });
        }
        return false;
      }
      this.client = client;
      client.on('messages', (batch: RpcMessage[]) => this.received(batch));
      client.on('close', () => this.lost(client));
      this.set({ state: 'connected', socket, serverVersion, appVersion: this.deps.appVersion });
      return true;
    } catch (error) {
      client.close();
      this.set({ state: 'unreachable', error: (error as Error).message });
      return false;
    }
  }

  private received(batch: RpcMessage[]) {
    for (const message of batch) {
      if (message.method !== 'serve/updating') continue;
      const version = (message.params as { version?: string } | undefined)?.version;
      if (version && compareVersions(version, this.deps.appVersion) === 'newer') {
        // The server stops for a newer Koda, now or once its turns finish.
        // This app can't use the new one: it must be started again.
        this.updatedTo = version;
        this.set({ state: 'updated', version, appVersion: this.deps.appVersion });
      }
    }
    this.emit('messages', batch);
  }

  private lost(client: KodaClient) {
    if (client !== this.client) return;
    this.client = undefined;
    if (this.closing || this.updatedTo) return;
    void this.reconnect('the server closed the connection');
  }

  /** Try to get back on Codex's schedule, starting a server when none answers. */
  private async reconnect(why: string): Promise<void> {
    this.set({ state: 'reconnecting', error: why });
    const budget = this.deps.reconnectBudgetMs ?? RECONNECT_BUDGET_MS;
    const deadline = Date.now() + budget;
    let last = why;
    for (let attempt = 0; ; attempt++) {
      const wait = FIRST_WAITS[attempt] ?? LATER_WAIT;
      if (Date.now() + wait > deadline) break;
      await this.sleep(wait);
      if (this.closing) return;
      if (!this.deps.koda) {
        this.set({ state: 'noKoda' });
        return;
      }
      const answer = await this.start({ koda: this.deps.koda, env: this.deps.env, socket: this.deps.socket });
      // An older server still running turns can't be replaced without asking:
      // keep trying, as the TUI does with no one to ask.
      if (answer.status === 'busy') {
        last = answer.message ?? 'an older server is still running turns';
        this.set({ state: 'reconnecting', error: last });
        continue;
      }
      if (await this.settle(answer)) return;
      if (this.current.state === 'updated') return;
      last = answer.message ?? last;
      this.set({ state: 'reconnecting', error: last });
    }
    this.set({ state: 'unreachable', error: last });
  }

  /** Send a request to the server. */
  request(method: string, params: unknown): Promise<unknown> {
    if (!this.client) return Promise.reject(new Error("not connected to Koda's server"));
    return this.client.request(method, params);
  }

  /** Whether `id` is a request the server sent and nobody has answered. */
  isOpenRequest(id: RpcId): boolean {
    return this.client?.openServerRequests.has(id) ?? false;
  }

  respond(id: RpcId, result: unknown) {
    if (!this.client) throw new Error("not connected to Koda's server");
    this.client.respond(id, result);
  }

  close() {
    this.closing = true;
    this.client?.close();
  }
}
