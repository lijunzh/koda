// Back and Forward: where the window has been.
import { describe, expect, it } from 'vitest';
import { back, canGoBack, canGoForward, forget, forward, here, MAX_PLACES, START, visit } from '../src/renderer/history';

describe('history', () => {
  it('starts at the new-session screen with nowhere to go', () => {
    expect(here(START)).toBeNull();
    expect(canGoBack(START)).toBe(false);
    expect(canGoForward(START)).toBe(false);
    expect(back(START)).toBe(START);
    expect(forward(START)).toBe(START);
  });

  it('goes back and forward through the sessions shown', () => {
    let h = visit(visit(START, 'a'), 'b');
    expect(here(h)).toBe('b');
    h = back(h);
    expect(here(h)).toBe('a');
    expect(canGoForward(h)).toBe(true);
    h = back(h);
    expect(here(h)).toBeNull();
    expect(canGoBack(h)).toBe(false);
    h = forward(forward(h));
    expect(here(h)).toBe('b');
  });

  it('drops what was ahead once the user goes somewhere else', () => {
    let h = back(visit(visit(START, 'a'), 'b'));
    h = visit(h, 'c');
    expect(h.places).toEqual([null, 'a', 'c']);
    expect(canGoForward(h)).toBe(false);
  });

  it("doesn't count staying where it is", () => {
    const h = visit(START, 'a');
    expect(visit(h, 'a')).toBe(h);
  });

  it('remembers only so far back', () => {
    let h = START;
    for (let i = 0; i < MAX_PLACES + 10; i++) h = visit(h, `s${i}`);
    expect(h.places.length).toBe(MAX_PLACES);
    expect(here(h)).toBe(`s${MAX_PLACES + 9}`);
  });

  it('forgets a deleted session, and joins the visits on either side of it', () => {
    // a, b, a: with b gone, a twice in a row is a once.
    let h = visit(visit(visit(START, 'a'), 'b'), 'a');
    h = forget(h, 'b');
    expect(h.places).toEqual([null, 'a']);
    expect(here(h)).toBe('a');

    // Deleted while shown: the window is at the place before it.
    h = forget(visit(visit(START, 'a'), 'b'), 'b');
    expect(here(h)).toBe('a');
    expect(canGoForward(h)).toBe(false);

    // Deleted behind the place shown: the place shown stays.
    h = forget(visit(visit(START, 'a'), 'b'), 'a');
    expect(h.places).toEqual([null, 'b']);
    expect(here(h)).toBe('b');

    const untouched = visit(START, 'a');
    expect(forget(untouched, 'zzz')).toBe(untouched);
  });
});
