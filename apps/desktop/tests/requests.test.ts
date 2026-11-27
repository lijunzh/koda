// What the server waits on the user for, and the answers the main process lets through.
import { describe, expect, it } from 'vitest';
import { answerRefusal } from '../src/main/answers';
import { formContent, formFields, forSession, withRequests, without } from '../src/renderer/requests';

const ask = (id: number, sessionId = 's1', method = 'item/toolCall/requestApproval') => ({
  jsonrpc: '2.0' as const,
  id,
  method,
  params: { sessionId, tool: 'Bash', detail: 'rm -f a' },
});
const resolved = (requestId: number) => ({ jsonrpc: '2.0' as const, method: 'serverRequest/resolved', params: { requestId } });

describe('requests', () => {
  it('keeps what waits, in order, once each, until it is answered anywhere', () => {
    let pending = withRequests([], [ask(1), ask(2, 's2'), ask(1), { jsonrpc: '2.0', method: 'turn/started', params: {} }]);
    expect(pending.map((p) => p.id)).toEqual([1, 2]);
    expect(forSession([...pending], 's2').map((p) => p.id)).toEqual([2]);
    pending = withRequests(pending, [resolved(1)]);
    expect(pending.map((p) => p.id)).toEqual([2]);
    expect(without(pending, 2)).toEqual([]);
    const same = withRequests(pending, [resolved(99)]);
    expect(same).toBe(pending);
  });

  it("reads an MCP server's form and types its values", () => {
    const fields = formFields({
      type: 'object',
      properties: {
        name: { type: 'string', title: 'Name' },
        count: { type: 'integer' },
        ok: { type: 'boolean' },
        color: { type: 'string', enum: ['red', 'blue'] },
      },
      required: ['name'],
    });
    expect(fields.map((f) => [f.name, f.kind, f.required])).toEqual([
      ['name', 'string', true],
      ['count', 'integer', false],
      ['ok', 'boolean', false],
      ['color', 'enum', false],
    ]);
    expect(formContent(fields, { name: ' Ada ', count: '3', color: 'red' })).toEqual({ name: 'Ada', count: 3, ok: false, color: 'red' });
    expect(formContent(fields, { count: '3' })).toBeNull();
    expect(formContent(fields, { name: 'a', count: '3.5' })).toBeNull();
  });
});

describe('answers', () => {
  const approval = (allowForSession: boolean) => ({ method: 'item/toolCall/requestApproval', params: { allowForSession } });

  it('lets through an approval that fits the request', () => {
    expect(answerRefusal(approval(false), { decision: 'accept' })).toBeNull();
    expect(answerRefusal(approval(false), { decision: 'cancel' })).toBeNull();
    expect(answerRefusal(approval(false), { decision: { declineWithFeedback: { feedback: 'use git rm' } } })).toBeNull();
    expect(answerRefusal(approval(true), { decision: 'acceptForSession' })).toBeNull();
  });

  it('refuses one that does not', () => {
    expect(answerRefusal(approval(false), { decision: 'acceptForSession' })).toMatch(/for the session/);
    expect(answerRefusal(approval(false), { decision: 'approve' })).toMatch(/not a decision/);
    expect(answerRefusal(approval(false), {})).toMatch(/not a decision/);
    expect(answerRefusal(undefined, { decision: 'accept' })).toMatch(/no such request/);
  });

  it('checks questions, the loop cap and MCP requests too', () => {
    expect(answerRefusal({ method: 'item/tool/requestUserInput', params: {} }, { answer: 'yes' })).toBeNull();
    expect(answerRefusal({ method: 'item/tool/requestUserInput', params: {} }, { answer: 3 })).not.toBeNull();
    expect(answerRefusal({ method: 'turn/loopCap/requestDecision', params: {} }, { decision: 'continue50' })).toBeNull();
    expect(answerRefusal({ method: 'turn/loopCap/requestDecision', params: {} }, { decision: 'continue9000' })).not.toBeNull();
    const elicit = { method: 'mcpServer/elicitation/request', params: {} };
    expect(answerRefusal(elicit, { action: 'accept', content: { a: 1 } })).toBeNull();
    expect(answerRefusal(elicit, { action: 'accept', content: 'x' })).not.toBeNull();
    expect(answerRefusal({ method: 'something/else', params: {} }, {})).not.toBeNull();
  });
});
