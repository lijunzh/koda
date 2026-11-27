import { useEffect, useState } from 'react';
import type { ServerStatus } from './bridge';

/** Says which server the app reached, and whether it runs the app's version. */
export function App() {
  const [status, setStatus] = useState<ServerStatus | null>(null);
  useEffect(() => {
    void window.koda.status().then(setStatus);
    return window.koda.onStatus(setStatus);
  }, []);
  return (
    <main className="status">
      <h1>Koda</h1>
      <StatusLine status={status} />
    </main>
  );
}

function StatusLine({ status }: { status: ServerStatus | null }) {
  if (!status || status.state === 'connecting') return <p>Connecting to Koda's server…</p>;
  if (status.state === 'unreachable') {
    return (
      <p className="problem">
        No Koda server answers at <code>{status.socket}</code>. Run <code>koda</code> once, or{' '}
        <code>koda serve --start</code>, then reopen the app.
        <br />
        <span className="detail">{status.error}</span>
      </p>
    );
  }
  if (status.match === 'same') {
    return (
      <p>
        Connected to Koda {status.serverVersion} at <code>{status.socket}</code>.
      </p>
    );
  }
  return (
    <p className="problem">
      Koda's server runs {status.serverVersion}; this app is {status.appVersion}.{' '}
      {status.match === 'newer'
        ? 'Koda was updated: open the updated app.'
        : 'Run the same version of koda, so it replaces the older server.'}
    </p>
  );
}
