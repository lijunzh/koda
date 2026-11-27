// Where the server is, and how its version stands against the app's.
import { describe, expect, it } from 'vitest';
import { compareVersions, defaultSocket } from '../src/main/server';

describe('server', () => {
  it("finds Koda's socket as koda does", () => {
    expect(defaultSocket({ HOME: '/Users/me' })).toBe('/Users/me/.config/koda/serve/koda.sock');
    expect(defaultSocket({ HOME: '/Users/me', XDG_CONFIG_HOME: '/x' })).toBe('/x/koda/serve/koda.sock');
  });

  it("compares versions as koda_client::version does", () => {
    expect(compareVersions('0.7.13', '0.7.13')).toBe('same');
    expect(compareVersions('0.7.12', '0.7.13')).toBe('older');
    expect(compareVersions('0.7.14', '0.7.13')).toBe('newer');
    expect(compareVersions('0.10.0', '0.9.9')).toBe('newer');
    expect(compareVersions('1.0.0', '0.99.0')).toBe('newer');
    // Another build of the same release is replaced, never kept.
    expect(compareVersions('0.7.13-dev', '0.7.13')).toBe('older');
    expect(compareVersions('0.7.13', '0.7.13+local')).toBe('older');
    expect(compareVersions('0.8.0-rc.1', '0.7.13')).toBe('newer');
    // What can't be read is replaced.
    expect(compareVersions('koda', '0.7.13')).toBe('older');
    expect(compareVersions('0.7', '0.7.13')).toBe('older');
  });
});
