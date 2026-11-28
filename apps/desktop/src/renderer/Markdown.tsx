// An agent's text as Markdown (react-markdown, GitHub's tables and lists,
// highlighted code). No raw HTML: react-markdown leaves it as text. A reply
// is split into chunks at block boundaries and finished chunks are kept, so
// a streaming reply re-parses only its last chunk on each update (from the
// Electron spike).
import { memo, type ReactNode, useRef, useState } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';

const remarkPlugins = [remarkGfm];
const rehypePlugins = [[rehypeHighlight, { detect: false, plainText: ['diff'] }]] as never[];

/** Copies `text`, and says so for a moment. */
export function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="copy"
      aria-label={label}
      title={label}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(
          () => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          },
          () => {},
        );
      }}
    >
      {copied ? 'Copied' : 'Copy'}
    </button>
  );
}

/** The text inside a rendered code block. */
function textOf(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(textOf).join('');
  if (node && typeof node === 'object' && 'props' in node) return textOf((node as { props: { children?: ReactNode } }).props.children);
  return '';
}

const components: Components = {
  // A code block, with a button that copies it.
  pre({ children, node: _node, ...rest }) {
    return (
      <div className="code-block">
        <CopyButton text={textOf(children).replace(/\n$/, '')} label="Copy code" />
        <pre {...rest}>{children}</pre>
      </div>
    );
  },
  code({ className, children, node: _node, ...rest }) {
    if (className?.includes('language-diff')) return <DiffText text={String(children)} />;
    return (
      <code className={className} {...rest}>
        {children}
      </code>
    );
  },
  // Links open in the browser (the main process allows only http and https),
  // and say where they go.
  a({ href, children }) {
    let host = '';
    try {
      host = href ? new URL(href).host : '';
    } catch {
      // Not a full URL: shown, not followed.
    }
    if (!host) return <span>{children}</span>;
    return (
      <a href={href} target="_blank" rel="noreferrer" title={href}>
        {children}
      </a>
    );
  },
  // The window loads nothing from outside; an image is its description.
  img({ alt, src }) {
    return <span className="md-image" title={src}>[image{alt ? `: ${alt}` : ''}]</span>;
  },
};

/** A unified diff's lines, coloured. */
export function DiffText({ text }: { text: string }) {
  return (
    <code className="diff">
      {text.split('\n').map((line, i) => {
        const tone =
          line.startsWith('+++') || line.startsWith('---')
            ? 'diff-file'
            : line.startsWith('@@')
              ? 'diff-hunk'
              : line.startsWith('+')
                ? 'diff-add'
                : line.startsWith('-')
                  ? 'diff-del'
                  : '';
        return (
          <span key={i} className={tone}>
            {line}
            {'\n'}
          </span>
        );
      })}
    </code>
  );
}

const Chunk = memo(function Chunk({ text }: { text: string }) {
  return (
    <ReactMarkdown remarkPlugins={remarkPlugins} rehypePlugins={rehypePlugins} components={components}>
      {text}
    </ReactMarkdown>
  );
});

/** A chunk closes at the first safe boundary past this many characters. */
const CHUNK_MIN = 1500;

interface Split {
  text: string;
  /** Where closed chunks end; text after the last is the open tail. */
  ends: number[];
  /** Where to go on scanning: the start of the line after the last one scanned. */
  scanFrom: number;
  inFence: boolean;
  fence: string;
  lastBlank: boolean;
}

const freshSplit = (): Split => ({ text: '', ends: [], scanFrom: 0, inFence: false, fence: '', lastBlank: false });

/** Extend a split over text that grew at its end; only the new lines are scanned. */
export function extendSplit(split: Split, text: string): Split {
  const s = text.startsWith(split.text) ? split : freshSplit();
  s.text = text;
  let pos = s.scanFrom;
  for (;;) {
    const nl = text.indexOf('\n', pos);
    if (nl < 0) break;
    const line = text.slice(pos, nl);
    const fence = /^ {0,3}(`{3,}|~{3,})/.exec(line);
    if (s.inFence) {
      if (fence && fence[1][0] === s.fence[0] && fence[1].length >= s.fence.length && !line.slice(fence[0].length).trim()) {
        s.inFence = false;
      }
      s.lastBlank = false;
    } else {
      const blank = line.trim() === '';
      // A boundary: a line that isn't indented, right after a blank one, outside code.
      if (!blank && s.lastBlank && !/^[ \t]/.test(line) && pos - (s.ends.at(-1) ?? 0) >= CHUNK_MIN) s.ends.push(pos);
      if (fence) {
        s.inFence = true;
        s.fence = fence[1];
      }
      s.lastBlank = blank;
    }
    pos = nl + 1;
  }
  s.scanFrom = pos;
  return s;
}

export function chunksOf(split: Split): string[] {
  const out: string[] = [];
  let start = 0;
  for (const end of split.ends) {
    out.push(split.text.slice(start, end));
    start = end;
  }
  if (start < split.text.length) out.push(split.text.slice(start));
  return out;
}

export const Markdown = memo(function Markdown({ text }: { text: string }) {
  const split = useRef<Split>(freshSplit());
  split.current = extendSplit(split.current, text);
  return (
    <div className="md">
      {chunksOf(split.current).map((chunk, i) => (
        <Chunk key={i} text={chunk} />
      ))}
    </div>
  );
});
