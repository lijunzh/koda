// What Koda is waiting on you for, above the prompt box: an approval, a
// question, the loop cap, or an MCP server's request. The choices are the
// TUI's (y, s, n, f, esc), as buttons.
import { useEffect, useState } from 'react';
import { DiffText } from './Markdown';
import { formContent, formFields, type Pending } from './requests';

interface Props {
  requests: Pending[];
  onAnswer(id: number | string, result: unknown): Promise<void>;
}

/** The oldest request first: one at a time, as the TUI asks them. */
export function Requests({ requests, onAnswer }: Props) {
  const first = requests[0];
  if (!first) return null;
  return (
    <div className="requests" data-testid="request">
      {requests.length > 1 && <div className="meta">{requests.length - 1} more waiting</div>}
      <RequestCard key={String(first.id)} request={first} onAnswer={(result) => onAnswer(first.id, result)} />
    </div>
  );
}

function RequestCard({ request, onAnswer }: { request: Pending; onAnswer(result: unknown): Promise<void> }) {
  const [error, setError] = useState<string | null>(null);
  const send = (result: unknown) => onAnswer(result).catch((e: Error) => setError(e.message));
  const card = (() => {
    switch (request.method) {
      case 'item/toolCall/requestApproval':
        return <Approval params={request.params} send={send} />;
      case 'item/tool/requestUserInput':
        return <Question params={request.params} send={send} />;
      case 'turn/loopCap/requestDecision':
        return <LoopCap params={request.params} send={send} />;
      case 'mcpServer/elicitation/request':
        return <Elicitation params={request.params} send={send} />;
    }
  })();
  return (
    <>
      <div className="needs-input">
        <span className="needs-dot" aria-hidden />
        Needs your input
      </div>
      <div className="request-card">
        {card}
        {error && <p className="problem">{error}</p>}
      </div>
    </>
  );
}

type Send = (result: unknown) => Promise<void>;

/** Esc answers what the TUI's Esc does. */
function useEscape(onEscape: () => void) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onEscape();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  });
}

function Approval({ params, send }: { params: Extract<Pending, { method: 'item/toolCall/requestApproval' }>['params']; send: Send }) {
  const [feedback, setFeedback] = useState<string | null>(null);
  // Esc stops the turn, or for a sub-agent's request refuses just that call.
  useEscape(() => void send({ decision: params.agent ? 'decline' : 'cancel' }));
  const what = params.command ?? params.detail;
  const forSession = params.sessionPrefix?.length ? params.sessionPrefix.join(' ') : 'this session';
  return (
    <>
      {params.agent && <div className="request-agent">🤖 {params.agent.name}{params.agent.id ? ` ${params.agent.id}` : ''} asks</div>}
      <div className="request-head">
        <strong>{params.tool}</strong> <code>{what}</code>
      </div>
      {params.outsideSandbox && (
        <div className="request-warning">
          ⚠ outside the sandbox{params.reason ? <span className="meta"> · why: {params.reason}</span> : null}
        </div>
      )}
      {params.networkApprovalContext && (
        <div className="request-warning">
          Wants to reach <code>{params.networkApprovalContext.host}</code>, which isn't on the allowed list
        </div>
      )}
      {!params.outsideSandbox && params.reason && <div className="meta">why: {params.reason}</div>}
      {(params.changes ?? []).map((change) => (
        <pre key={change.path} className="tool-diff">
          <DiffText text={change.diff} />
        </pre>
      ))}
      {feedback === null ? (
        <div className="actions">
          <button className="primary" onClick={() => void send({ decision: 'accept' })}>
            Yes
          </button>
          {params.allowForSession && (
            <button onClick={() => void send({ decision: 'acceptForSession' })}>
              Yes, and don't ask again for <code>{forSession}</code>
            </button>
          )}
          <button onClick={() => void send({ decision: 'decline' })}>No</button>
          {!params.background && <button onClick={() => setFeedback('')}>No, and tell Koda what to do instead</button>}
          <span className="meta">esc to {params.agent ? 'refuse' : 'stop'}</span>
        </div>
      ) : (
        <div className="actions column">
          <textarea
            autoFocus
            rows={2}
            aria-label="What Koda should do instead"
            value={feedback}
            onChange={(e) => setFeedback(e.target.value)}
          />
          <div className="actions">
            <button
              className="primary"
              disabled={!feedback.trim()}
              onClick={() => void send({ decision: { declineWithFeedback: { feedback: feedback.trim() } } })}
            >
              Send
            </button>
            <button onClick={() => setFeedback(null)}>Back</button>
          </div>
        </div>
      )}
    </>
  );
}

/** The model's question, as Claude Code's desktop app asks one: a choice per row, "Other" to type one, Skip and Submit. */
function Question({ params, send }: { params: Extract<Pending, { method: 'item/tool/requestUserInput' }>['params']; send: Send }) {
  const options = params.options;
  // The picked option's index; `options.length` is "Other".
  const [picked, setPicked] = useState<number | null>(options.length ? null : 0);
  const [other, setOther] = useState('');
  const answer = picked === null ? '' : picked < options.length ? options[picked] : other.trim();
  const submit = () => {
    if (answer) void send({ answer });
  };
  useEscape(() => void send({ answer: '' }));
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        e.stopPropagation();
        submit();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  });
  return (
    <div className="question">
      <div className="request-head question-text">{params.question}</div>
      <div role="radiogroup" aria-label={params.question}>
        {options.map((option, i) => (
          <button key={option} role="radio" aria-checked={picked === i} className="choice" onClick={() => setPicked(i)}>
            <span className="radio" aria-hidden />
            {option}
          </button>
        ))}
        <div className={options.length ? 'choice other' : 'choice other only'}>
          {options.length > 0 && (
            <button role="radio" aria-checked={picked === options.length} aria-label="Other" className="radio-only" onClick={() => setPicked(options.length)}>
              <span className="radio" aria-hidden />
            </button>
          )}
          <input
            aria-label="Your answer"
            placeholder={options.length ? 'Other' : 'Type your answer'}
            value={other}
            onFocus={() => setPicked(options.length)}
            onChange={(e) => setOther(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.metaKey && !e.ctrlKey && other.trim() && !e.nativeEvent.isComposing) {
                void send({ answer: other.trim() });
              }
            }}
          />
        </div>
      </div>
      <div className="actions end">
        <button onClick={() => void send({ answer: '' })} title="Esc">
          Skip
        </button>
        <button className="primary" disabled={!answer} onClick={submit}>
          Submit<span className="key-hint"> ⌘↵</span>
        </button>
      </div>
    </div>
  );
}

function LoopCap({ params, send }: { params: Extract<Pending, { method: 'turn/loopCap/requestDecision' }>['params']; send: Send }) {
  useEscape(() => void send({ decision: 'stop' }));
  return (
    <>
      <div className="request-head">
        ⚠ Koda has taken {params.cap} steps in this turn. Go on?
      </div>
      {params.recentTools.length > 0 && <div className="meta">Lately: {params.recentTools.join(', ')}</div>}
      <div className="actions">
        <button className="primary" onClick={() => void send({ decision: 'continue50' })}>
          50 more
        </button>
        <button onClick={() => void send({ decision: 'continue200' })}>200 more</button>
        <button onClick={() => void send({ decision: 'stop' })}>Stop</button>
      </div>
    </>
  );
}

function Elicitation({
  params,
  send,
}: {
  params: Extract<Pending, { method: 'mcpServer/elicitation/request' }>['params'];
  send: Send;
}) {
  const fields = params.url ? [] : formFields(params.requestedSchema);
  const [values, setValues] = useState<Record<string, string | boolean>>({});
  useEscape(() => void send({ action: 'cancel' }));
  let host = '';
  try {
    host = params.url ? new URL(params.url).host : '';
  } catch {
    // Not a URL: shown as text.
  }
  const content = formContent(fields, values);
  return (
    <>
      <div className="request-head">
        <strong>{params.serverName}</strong> {params.message}
      </div>
      {params.url ? (
        <p>
          It asks you to open{' '}
          <a href={params.url} target="_blank" rel="noreferrer" title={params.url}>
            {host || params.url}
          </a>
          , then say when you're done.
        </p>
      ) : (
        <div className="form">
          {fields.map((field) => (
            <label key={field.name} title={field.description}>
              <span>
                {field.title}
                {field.required ? ' *' : ''}
              </span>
              {field.kind === 'boolean' ? (
                <input
                  type="checkbox"
                  checked={Boolean(values[field.name])}
                  onChange={(e) => setValues({ ...values, [field.name]: e.target.checked })}
                />
              ) : field.kind === 'enum' ? (
                <select value={String(values[field.name] ?? '')} onChange={(e) => setValues({ ...values, [field.name]: e.target.value })}>
                  <option value="">—</option>
                  {field.options.map((o) => (
                    <option key={o}>{o}</option>
                  ))}
                </select>
              ) : (
                <input
                  inputMode={field.kind === 'string' ? undefined : 'decimal'}
                  value={String(values[field.name] ?? '')}
                  onChange={(e) => setValues({ ...values, [field.name]: e.target.value })}
                />
              )}
            </label>
          ))}
        </div>
      )}
      <div className="actions">
        <button
          className="primary"
          disabled={!params.url && content === null}
          onClick={() => void send(params.url ? { action: 'accept' } : { action: 'accept', content })}
        >
          {params.url ? 'Done' : 'Send'}
        </button>
        <button onClick={() => void send({ action: 'decline' })}>Decline</button>
        <button onClick={() => void send({ action: 'cancel' })}>Cancel</button>
      </div>
    </>
  );
}
