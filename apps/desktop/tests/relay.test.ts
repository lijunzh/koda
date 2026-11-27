// What the page may ask the server for.
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { describe, expect, it } from 'vitest';
import { RELAYED_METHODS, refusal } from '../src/main/relay';

const schema = JSON.parse(
  readFileSync(path.join(__dirname, '../../../docs/src/protocol-schema.json'), 'utf8'),
) as { methods: Record<string, unknown> };

describe('relay', () => {
  it('lets the page read sessions and drive a running turn', () => {
    expect(refusal('session/list', {})).toBeNull();
    expect(refusal('session/list', undefined)).toBeNull();
    expect(refusal('turn/interrupt', { sessionId: 's' })).toBeNull();
  });

  it("keeps the server's settings, keys and lifetime away from the page", () => {
    for (const method of [
      'config/batchWrite',
      'config/read',
      'modelProvider/apiKey/set',
      'webSearch/apiKey/set',
      'mcpServer/oauth/login',
      'session/delete',
      'session/memory/add',
      'feedback/save',
      'serve/stop',
      'initialize',
    ]) {
      expect(refusal(method, {}), method).toMatch(/isn't open to the page/);
    }
  });

  it('refuses what later steps add with their checks', () => {
    // Each needs a check of its parameters first (a folder, a mode, a config).
    for (const method of ['turn/start', 'session/start', 'session/resume', 'session/fork', 'session/settings/update']) {
      expect(refusal(method, {}), method).not.toBeNull();
    }
  });

  it('refuses a model list from another address, which would carry the stored key there', () => {
    expect(refusal('model/list', {})).toBeNull();
    expect(refusal('model/list', { modelProvider: 'anthropic' })).toBeNull();
    expect(refusal('model/list', { baseUrl: 'https://example.com/v1' })).toMatch(/baseUrl/);
  });

  it('refuses what is not a method and params that are not an object', () => {
    expect(refusal(42, {})).not.toBeNull();
    expect(refusal('session/list', [])).not.toBeNull();
    expect(refusal('session/list', 'x')).not.toBeNull();
  });

  it("names only methods Koda's protocol has", () => {
    for (const method of RELAYED_METHODS) {
      expect(schema.methods, method).toHaveProperty([method]);
    }
  });
});
