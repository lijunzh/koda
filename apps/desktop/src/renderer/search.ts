// Search's second half: the sessions in which the text was said
// (`session/search`), shown under the sessions whose title has it. Pure
// functions of the query and the server's answers; the sidebar asks, a
// moment after the typing stops, and shows what these make of the answer.
import type { Session, SessionSearchResponse } from '../protocol/types.gen';
import { listed } from './sessions';

/** A query shorter than this only narrows the list by title. */
export const MIN_QUERY = 2;
/** How long the typing has to rest before the server is asked. */
export const SEARCH_DELAY_MS = 250;
/** How many sessions a search asks for: the server's own default. */
export const SEARCH_LIMIT = 25;

/** One search: the text, and whether it looks in the archive instead. */
export interface Asked {
  term: string;
  archived: boolean;
}

/** A session the text was said in, with where. */
export interface Found {
  session: Session;
  /** The text around the first match, as the server cut it. */
  snippet: string;
  /** The item the match is in, to open the session at; old sessions have none. */
  itemId: string | null;
}

export interface SearchState {
  /** The latest search; `null` while the query is too short for one. */
  asked: Asked | null;
  /** Whether its answer is still to come. */
  waiting: boolean;
  found: readonly Found[];
  error: string | null;
}

export const IDLE: SearchState = { asked: null, waiting: false, found: [], error: null };

/** What to search for when the box holds `query`; `null` when that's too short. */
export function searchTerm(query: string | null): string | null {
  const term = query?.trim() ?? '';
  return term.length >= MIN_QUERY ? term : null;
}

/** A new search starts, or the query got too short for one: what was found before goes. */
export function asking(asked: Asked | null): SearchState {
  return asked ? { asked, waiting: true, found: [], error: null } : IDLE;
}

const same = (a: Asked | null, b: Asked) => a !== null && a.term === b.term && a.archived === b.archived;

/** The server's answer to `asked`. One for an earlier search is dropped. */
export function answered(state: SearchState, asked: Asked, response: SessionSearchResponse): SearchState {
  if (!same(state.asked, asked)) return state;
  const found = response.data
    .filter((result) => listed(result.session))
    .map((result) => ({ session: result.session, snippet: result.snippet, itemId: result.itemId ?? null }));
  return { asked, waiting: false, found, error: null };
}

/** The search for `asked` failed. A failure of an earlier search is dropped. */
export function failed(state: SearchState, asked: Asked, message: string): SearchState {
  return same(state.asked, asked) ? { asked, waiting: false, found: [], error: message } : state;
}

/** What "In conversations" lists: the sessions found that aren't shown above as title matches. */
export function unshown(found: readonly Found[], shown: ReadonlySet<string>): Found[] {
  return found.filter((result) => !shown.has(result.session.id));
}

/**
 * The line that says the list is empty, if it is: `titles` is how many
 * sessions the title filter left, `results` how many "In conversations"
 * shows. While a search waits, "Searching…" says it instead.
 */
export function emptyLine(query: string | null, titles: number, results: number, state: SearchState): string | null {
  if (titles > 0 || results > 0) return null;
  if (!query?.trim()) return 'No sessions yet.';
  if (!state.asked || state.error) return 'No session matches.';
  return state.waiting ? null : 'No conversation mentions it.';
}

/** A piece of a snippet; `hit` where it is the text searched for. */
export interface Part {
  text: string;
  hit: boolean;
}

/** `snippet` in pieces, each place `term` occurs (letters in either case) apart from the rest. */
export function marked(snippet: string, term: string): Part[] {
  const text = snippet.toLowerCase();
  const wanted = term.toLowerCase();
  // A letter whose lower case is longer would shift every place after it.
  if (!wanted || text.length !== snippet.length) return [{ text: snippet, hit: false }];
  const parts: Part[] = [];
  let from = 0;
  for (let at = text.indexOf(wanted); at >= 0; at = text.indexOf(wanted, from)) {
    if (at > from) parts.push({ text: snippet.slice(from, at), hit: false });
    from = at + wanted.length;
    parts.push({ text: snippet.slice(at, from), hit: true });
  }
  if (from < snippet.length) parts.push({ text: snippet.slice(from), hit: false });
  return parts;
}
