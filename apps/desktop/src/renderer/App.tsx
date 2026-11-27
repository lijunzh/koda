import { useEffect, useState } from 'react';
import type { ServerStatus } from './bridge';

/** The app's window. For now: which server it reached, and what to do when it can't use one. */
export function App() {
  const [status, setStatus] = useState<ServerStatus | null>(null);
  useEffect(() => {
    const stop = window.koda.onStatus(setStatus);
    void window.koda.status().then(setStatus);
    return stop;
  }, []);
  return (
    <main className="status">
      <h1>Koda</h1>
      <ServerLine status={status} />
    </main>
  );
}

function sessions(n: number) {
  return n === 1 ? '1 session' : `${n} sessions`;
}

/** What the server's state means, and the user's choices, as the TUI asks them. */
function ServerLine({ status }: { status: ServerStatus | null }) {
  if (!status || status.state === 'starting') return <p>Starting Koda's server…</p>;
  switch (status.state) {
    case 'connected':
      return (
        <p data-testid="connected">
          Connected to Koda {status.serverVersion} at <code>{status.socket}</code>.
        </p>
      );
    case 'reconnecting':
      return (
        <p className="problem" data-testid="reconnecting">
          Lost Koda's server; reconnecting… <span className="detail">{status.error}</span>
        </p>
      );
    case 'busy':
      return (
        <div className="banner" data-testid="busy">
          <p>
            Koda was updated to {status.appVersion}. Its server still runs {status.serverVersion}, with a turn
            running in {sessions(status.working)}.
          </p>
          <div className="actions">
            <button onClick={() => void window.koda.replaceServer('wait')}>Wait for them, then restart it</button>
            {!status.draining && (
              <button onClick={() => void window.koda.replaceServer('stop')}>
                Restart it now (those turns stop)
              </button>
            )}
          </div>
        </div>
      );
    case 'waiting':
      return (
        <p data-testid="waiting">
          Waiting for the turns running in {sessions(status.working)} to finish before Koda's server restarts…
        </p>
      );
    case 'updated':
      return (
        <div className="banner" data-testid="updated">
          <p>
            Koda was updated to {status.version}; this window is {status.appVersion}.
          </p>
          <div className="actions">
            <button onClick={() => void window.koda.relaunch()}>Restart now</button>
          </div>
        </div>
      );
    case 'noKoda':
      return (
        <div className="banner problem" data-testid="no-koda">
          <p>
            No <code>koda</code> was found on your shell's PATH. Install it (<code>brew tap lijunzh/koda</code>, then <code>brew install koda</code>),
            then try again.
          </p>
          <div className="actions">
            <button onClick={() => void window.koda.retry()}>Try again</button>
          </div>
        </div>
      );
    case 'unreachable':
      return (
        <div className="banner problem" data-testid="unreachable">
          <p>Koda's server couldn't be started or reached.</p>
          <p className="detail">{status.error}</p>
          <div className="actions">
            <button onClick={() => void window.koda.retry()}>Try again</button>
          </div>
        </div>
      );
  }
}
