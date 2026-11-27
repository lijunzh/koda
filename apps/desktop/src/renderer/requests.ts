// What the server is waiting on the user for: tool approvals, the model's
// questions, the loop cap and MCP servers' requests. They come as requests
// with an id; one answered anywhere (this window, another, the TUI) is
// withdrawn with `serverRequest/resolved`, and opening a session sends again
// those still waiting.
import type {
  LoopCapParams,
  McpServerElicitationRequestParams,
  ToolCallApprovalParams,
  UserInputParams,
} from '../protocol/types.gen';
import type { RpcMessage } from './bridge';

export type Pending =
  | { id: number | string; method: 'item/toolCall/requestApproval'; params: ToolCallApprovalParams }
  | { id: number | string; method: 'item/tool/requestUserInput'; params: UserInputParams }
  | { id: number | string; method: 'turn/loopCap/requestDecision'; params: LoopCapParams }
  | { id: number | string; method: 'mcpServer/elicitation/request'; params: McpServerElicitationRequestParams };

const METHODS = new Set<string>([
  'item/toolCall/requestApproval',
  'item/tool/requestUserInput',
  'turn/loopCap/requestDecision',
  'mcpServer/elicitation/request',
]);

/** The requests still waiting after `batch`, in the order they came. */
export function withRequests(pending: readonly Pending[], batch: readonly RpcMessage[]): readonly Pending[] {
  let next: Pending[] | null = null;
  const edit = () => (next ??= pending.slice());
  for (const message of batch) {
    if (message.method && message.id !== undefined && METHODS.has(message.method)) {
      const current = next ?? pending;
      if (!current.some((p) => p.id === message.id)) edit().push(message as unknown as Pending);
    } else if (message.method === 'serverRequest/resolved') {
      const answered = (message.params as { requestId?: number | string } | undefined)?.requestId;
      const at = (next ?? pending).findIndex((p) => p.id === answered);
      if (at >= 0) edit().splice(at, 1);
    }
  }
  return next ?? pending;
}

/** Without request `id`, once this window answered it. */
export function without(pending: readonly Pending[], id: number | string): readonly Pending[] {
  return pending.filter((p) => p.id !== id);
}

/** The requests of session `sessionId`. */
export function forSession(pending: readonly Pending[], sessionId: string): Pending[] {
  return pending.filter((p) => p.params.sessionId === sessionId);
}

/** A form's fields, from the flat schema an MCP server asks with. */
export interface Field {
  name: string;
  title: string;
  kind: 'string' | 'number' | 'integer' | 'boolean' | 'enum';
  options: string[];
  required: boolean;
  description: string;
}

export function formFields(schema: unknown): Field[] {
  const s = (schema ?? {}) as { properties?: Record<string, Record<string, unknown>>; required?: string[] };
  return Object.entries(s.properties ?? {}).map(([name, p]) => {
    const options = Array.isArray(p.enum) ? (p.enum as unknown[]).map(String) : [];
    const type = String(p.type ?? 'string');
    const kind: Field['kind'] = options.length
      ? 'enum'
      : type === 'number' || type === 'integer' || type === 'boolean'
        ? type
        : 'string';
    return {
      name,
      title: typeof p.title === 'string' ? p.title : name,
      kind,
      options,
      required: (s.required ?? []).includes(name),
      description: typeof p.description === 'string' ? p.description : '',
    };
  });
}

/** A form's values typed as its schema says; `null` if a required one is missing or a number isn't one. */
export function formContent(fields: readonly Field[], values: Record<string, string | boolean>): Record<string, unknown> | null {
  const content: Record<string, unknown> = {};
  for (const field of fields) {
    const value = values[field.name];
    if (field.kind === 'boolean') {
      content[field.name] = Boolean(value);
      continue;
    }
    const text = typeof value === 'string' ? value.trim() : '';
    if (!text) {
      if (field.required) return null;
      continue;
    }
    if (field.kind === 'number' || field.kind === 'integer') {
      const n = Number(text);
      if (!Number.isFinite(n) || (field.kind === 'integer' && !Number.isInteger(n))) return null;
      content[field.name] = n;
    } else {
      content[field.name] = text;
    }
  }
  return content;
}
