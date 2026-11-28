// The only bridge between the page and the main process. The page gets these
// functions and no Node or Electron object (sandbox and context isolation
// on); the main process checks every call (`src/main/relay.ts`).
import { contextBridge, ipcRenderer, type IpcRendererEvent } from 'electron';

contextBridge.exposeInMainWorld('koda', {
  /** Where the server is and whether it runs this app's version. */
  status: () => ipcRenderer.invoke('koda:status'),
  /** Called with the server status whenever it changes. Returns a function that stops it. */
  onStatus: (listener: (status: unknown) => void) => {
    const handler = (_event: IpcRendererEvent, status: unknown) => listener(status);
    ipcRenderer.on('koda:status', handler);
    return () => ipcRenderer.removeListener('koda:status', handler);
  },
  /** Send a request; resolves with its result or rejects with why it failed or was refused. */
  request: (method: string, params: unknown) => ipcRenderer.invoke('koda:request', method, params),
  /** Answer a request the server sent, such as an approval. */
  respond: (id: number | string, result: unknown) => ipcRenderer.invoke('koda:respond', id, result),
  /** Replace an older server whose turns are running: `wait` for them, or `stop` them. */
  replaceServer: (turns: 'wait' | 'stop') => ipcRenderer.invoke('koda:replaceServer', turns),
  /** Ask the user for a folder in the system's dialog; resolves with it, or null if they cancelled. */
  pickFolder: () => ipcRenderer.invoke('koda:pickFolder'),
  /** A new session in a folder the user picked or that has a session, on a provider the server lists. */
  startSession: (options: { folder: string; provider?: string; model?: string }) =>
    ipcRenderer.invoke('koda:startSession', options),
  /** Delete a session once the user confirms; resolves with whether it was deleted. */
  deleteSession: (id: string) => ipcRenderer.invoke('koda:deleteSession', id),
  /** Put a session in Auto (after the user confirms) or Plan; resolves with whether it changed. */
  setMode: (id: string, mode: 'auto' | 'plan') => ipcRenderer.invoke('koda:setMode', id, mode),
  /** What Settings says the main process should do: send notifications, keep the computer awake. */
  setAppPrefs: (prefs: { notifications: boolean; keepAwake: boolean }) => ipcRenderer.invoke('koda:setAppPrefs', prefs),
  /** Try starting or joining the server again. */
  retry: () => ipcRenderer.invoke('koda:retry'),
  /** Quit and open the app again: after an update, the new one. */
  relaunch: () => ipcRenderer.invoke('koda:relaunch'),
  /** Notifications and server requests, in batches, in order. Returns a function that stops it. */
  onMessages: (listener: (batch: unknown[]) => void) => {
    const handler = (_event: IpcRendererEvent, batch: unknown[]) => listener(batch);
    ipcRenderer.on('koda:messages', handler);
    return () => ipcRenderer.removeListener('koda:messages', handler);
  },
});
