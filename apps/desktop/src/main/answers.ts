// Answers to the server's requests, which the page asks for and the main
// process sends only when they fit the request: an approval's yes or no
// (and "for the session" only when the request offered it), a question's
// answer, the loop cap's choice, an MCP server's form. The server takes an
// answer from any client, so an answer that doesn't fit is refused here.

export interface OpenRequest {
  method: string;
  params: Record<string, unknown>;
}

const DECISIONS = new Set(['accept', 'acceptForSession', 'decline', 'cancel']);
const LOOP_CAP = new Set(['continue50', 'continue200', 'stop']);
const ELICITATION = new Set(['accept', 'decline', 'cancel']);

/** Why `result` can't answer `request`, or `null` if it can. */
export function answerRefusal(request: OpenRequest | undefined, result: unknown): string | null {
  if (!request) return 'no such request is waiting';
  const r = (result ?? {}) as Record<string, unknown>;
  switch (request.method) {
    case 'item/toolCall/requestApproval': {
      const d = r.decision;
      if (typeof d === 'string') {
        if (!DECISIONS.has(d)) return `not a decision: ${d}`;
        if (d === 'acceptForSession' && !request.params.allowForSession) return 'this request has no "for the session"';
        return null;
      }
      const feedback = (d as { declineWithFeedback?: { feedback?: unknown } } | null)?.declineWithFeedback?.feedback;
      return typeof feedback === 'string' ? null : 'not a decision';
    }
    case 'item/tool/requestUserInput':
      return typeof r.answer === 'string' ? null : 'an answer is text';
    case 'turn/loopCap/requestDecision':
      return typeof r.decision === 'string' && LOOP_CAP.has(r.decision) ? null : 'not a loop-cap decision';
    case 'mcpServer/elicitation/request':
      if (typeof r.action !== 'string' || !ELICITATION.has(r.action)) return 'not an elicitation action';
      if (r.action === 'accept' && r.content !== undefined && (typeof r.content !== 'object' || r.content === null)) {
        return "a form's content is an object";
      }
      return null;
    default:
      return `${request.method} isn't answered from the page`;
  }
}
