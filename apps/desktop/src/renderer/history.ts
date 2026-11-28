// Where the window has been, for Back and Forward (Cmd+[ and Cmd+], as in
// Claude Code's desktop app): the sessions shown in the focused pane, in the
// order they were shown. `null` is the new-session screen.

export type Place = string | null;

export interface History {
  places: readonly Place[];
  /** The place shown now. */
  at: number;
}

/** How far back the window remembers. */
export const MAX_PLACES = 50;

export const START: History = { places: [null], at: 0 };

export const here = (history: History): Place => history.places[history.at] ?? null;
export const canGoBack = (history: History) => history.at > 0;
export const canGoForward = (history: History) => history.at < history.places.length - 1;

/** Having gone to `place`: what was ahead is dropped, as a browser does. */
export function visit(history: History, place: Place): History {
  if (here(history) === place) return history;
  const places = [...history.places.slice(0, history.at + 1), place].slice(-MAX_PLACES);
  return { places, at: places.length - 1 };
}

export function back(history: History): History {
  return canGoBack(history) ? { ...history, at: history.at - 1 } : history;
}

export function forward(history: History): History {
  return canGoForward(history) ? { ...history, at: history.at + 1 } : history;
}

/** Without a session that is gone (deleted): Back never lands on it. */
export function forget(history: History, id: string): History {
  if (!history.places.includes(id)) return history;
  const places: Place[] = [];
  let at = 0;
  history.places.forEach((place, i) => {
    // Two visits to one place, once the session between them is gone, are one.
    if (place !== id && places.at(-1) !== place) places.push(place);
    if (i === history.at) at = Math.max(0, places.length - 1);
  });
  if (!places.length) return START;
  return { places, at };
}
