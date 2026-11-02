# Debugging a run

This page is for contributors, who are often AI coding agents working from an
issue such as [#1473](https://github.com/lijunzh/koda/issues/1473). It shows how
to work out what a run did from what the run left behind, and how to turn that
into a bug report or a fix. None of it needs an API key.

If Koda crashed or left your terminal broken, start with
[Troubleshooting](./troubleshooting.md).

## What a run leaves behind

Below, `<config>` is `$XDG_CONFIG_HOME/koda`, which is `~/.config/koda` unless
you set that variable.

| Artifact | Where | What it holds |
|---|---|---|
| Log | `<config>/logs/koda-<pid>.log` | One file per process. `latest` in the same folder is a symlink to the newest (Unix only). Startup keeps the 50 newest and deletes the rest. |
| Panic log | `<config>/logs/panic.log` | Crash records from every process. See [Troubleshooting](./troubleshooting.md). |
| Database | `<config>/db/koda.db` | Every message of every session, with token usage. |
| Debug bundle | `<config>/debug-bundles/*.zip` | Written by [`/debug-bundle`](./commands.md#debug-bundle) in the TUI. |
| Headless output | stdout | A run summary, with `--output-format json` or `stream-json`. |

### The database

`koda.db` is plain SQLite. Open it from your own shell: the sandbox denies
sandboxed commands any access to `<config>/db`, so a tool call inside Koda can't
read it.

```sql
SELECT id, role, length(content) AS chars, prompt_tokens, completion_tokens,
       cache_read_tokens, thinking_tokens, agent_name, compacted_at
FROM messages
WHERE session_id = '<id>'
ORDER BY id;
```

Get the session ID from `run_summary`, or list the newest with
`SELECT id, title, created_at FROM sessions ORDER BY created_at DESC LIMIT 5;`.

- Compaction never deletes rows. It sets `compacted_at` on the rows it replaced
  and adds the summary as a `role = 'system'` row, which carries the usage of
  the summary call. Rows stay until `/purge` or `/sessions delete`.
- `compacted_at IS NULL` is close to what the next request will send, but a
  load can still hide some of those rows. See `history edited` below.
- `content` is what the model gets. For a tool result, `full_content` can hold
  the longer output that was cut from `content`. A microcompact overwrites
  `content` in place with `[Old tool result content cleared]`.
- Usage is per row, on assistant rows and summary rows.

> **Never attach `koda.db` to an issue.** It holds every session you have run,
> and API keys saved with `/key` sit in its `kv_store` table in plain text.

### Headless output

`--output-format json` prints one `run_summary` object when the run ends:
`success`, `error`, `session_id`, `provider`, `model`, `response`, `turn_end`,
`elapsed_ms`, `usage`, `usage_by_agent`, `tool_calls` and
`sub_agent_tool_calls`. `stream-json` prints the [wire protocol](headless.md#wire-protocol) as lines, then
the same summary as the last line. See
[Headless mode](./headless.md#output-formats). A run that failed exits with
code 1.

`turn_end.kind` is `complete`, `cancelled` or `error`, which is coarser than the
log. A turn stopped by the loop guard or the iteration cap reports `complete`
here, and only the `turn end` log line says otherwise.

## The log lines

Each line is a timestamp, a level, a target and a message, then `name=value`
fields:

```text
2026-11-01T12:19:14.608345Z  INFO koda_core::inference_helpers: model call purpose="turn" system_hash="7d3978775a2b4855" tools_hash="21353a598ffba47b" provider="openai-compat" model="stub" messages=2 estimated_tokens=9058 context_tokens=1000 first_chunk_ms=2 elapsed_ms=2 prompt_tokens=1000 completion_tokens=10 cache_read_tokens=0 thinking_tokens=0 stop_reason=tool_calls failed_stop=false tool_calls=1 text_chars=0
```

These five lines are on at the default level and hold no conversation text.
Tests pin them field by field, so they are safe to grep:

```sh
grep -E ' (model call|context assembled|compaction|history edited|turn end)' \
  ~/.config/koda/logs/latest
```

Read a run as a sequence: `context assembled`, then `model call`, once per
iteration, with `compaction` lines between them when one fires, and one
`turn end` at the end.

Sub-agents run their own loop and don't write these lines. For their cost, use
`usage_by_agent` and `sub_agent_tool_calls` in `run_summary`.

### `model call`

One line per completed call to the model. Written in `ModelCall::log`
(`koda-core/src/inference_helpers.rs`).

| Field | Meaning |
|---|---|
| `purpose` | `turn` or `compaction` |
| `system_hash`, `tools_hash` | Fingerprints of the system prompt and tool definitions sent. Turn calls only. Comparable within one build, not across builds. |
| `provider`, `model` | The provider implementation (`openai-compat` for any OpenAI-compatible server) and the model id requested |
| `messages` | Messages sent |
| `estimated_tokens` | Koda's estimate of the prompt, before any correction |
| `context_tokens` | What the provider says the request put in the window. `0` if it reported nothing. |
| `first_chunk_ms`, `elapsed_ms` | Time to the first stream chunk (turn calls only) and to the end of the call |
| `prompt_tokens`, `completion_tokens`, `cache_read_tokens`, `thinking_tokens` | Usage as reported |
| `stop_reason` | Why the model stopped, lowercased. `max_tokens` means it was cut off. |
| `failed_stop` | `true` when the stop reason means the model failed to answer, such as `malformed_function_call` or `safety`. See `is_failed_stop`. |
| `tool_calls`, `text_chars` | What the reply held |

An empty `stop_reason` means Koda has none, so don't read it as a clean stop.

A call that returns an error writes `model call failed` at `warn` instead, with
`purpose`, `provider`, `model`, `messages`, `elapsed_ms` and `error`.

### `context assembled`

One line each time the context is built for a call, and again after a
compaction rebuilds it. The last one before a `model call` describes that call.
Written in `assemble_context` (`koda-core/src/inference.rs`).

| Field | Meaning |
|---|---|
| `iteration` | Loop iteration within the turn |
| `estimated_tokens` | Messages, tool definitions and `correction` |
| `correction` | Tokens added because the provider's last count came in above the estimate. Never negative. |
| `window`, `percent` | The context window, and how full this estimate makes it |
| `messages` | Messages assembled |
| `system_chars`, `user_chars`, `assistant_chars`, `tool_chars`, `tool_calls_chars`, `reasoning_chars`, `images` | Size by role |
| `summary_chars` | Stored compaction summaries in the history |
| `largest_index`, `largest_role`, `largest_chars` | The largest message |
| `duplicate_read_files`, `duplicate_read_tokens` | Files read more than once in the current history, and what the extra reads cost |

### `compaction`

One line per compaction that changed the history. Written in
`koda-core/src/compact.rs`.

| Field | Meaning |
|---|---|
| `branch` | What happened to the reply, below |
| `rows_summarized`, `rows_kept`, `rows_archived` | Rows replaced, rows kept verbatim, rows marked `compacted_at` |
| `conversation_chars`, `prompt_chars` | Text to summarize, and the prompt built from it |
| `truncated` | The oldest messages were dropped to fit the summary call |
| `clipped_chars` | Characters the summarizer didn't see: each message outside a summary is cut to 2,000 characters and its tool calls to 500 |
| `attempts` | Calls made. A summary that was cut off is retried once with more room. |
| `reply_chars`, `stop_reason` | The last reply |
| `summary_chars` | The stored text, including the user's carried messages |
| `consecutive_failures` | Toward the circuit breaker, which stops summary calls at 3 |

The `dropped` branch makes no model call, so its line has only the `rows_*`,
`summary_chars` and `consecutive_failures` fields.

| `branch` | Stored | Meaning |
|---|---|---|
| `summary` | Summary | A complete `<summary>` block |
| `untagged` | Reply | Plain text with no tags, accepted as it is |
| `analysis` | Reply | The reply finished without a usable `<summary>`, so its text is kept |
| `partial` | Partial summary | Still cut off with no room left to retry |
| `cut_off` | Marker | Cut off with nothing usable |
| `empty` | Marker | An empty reply |
| `too_short` | Marker | Under 8 characters |
| `dropped` | Marker | No summary call: the context was past the window |

The marker is `(no summary available)`, stored with the user's messages and any
earlier summary from the part it replaced.

### `history edited`

A sanitization pass hid rows from the model when the history was loaded. The
rows stay in the database, so this line is the only trace. It is a `warn` when
text was lost and an `info` when it wasn't, and it repeats on each load while
the rows are there. Written in `koda-core/src/db/queries.rs`.

| Field | Meaning |
|---|---|
| `session_id` | The session |
| `pass` | `prune_legacy_compaction_hint`, `prune_mismatched_tool_calls`, `prune_null_content_messages`, `prune_whitespace_only_messages` or `dedupe_tool_results_by_call_id` |
| `rows`, `tool_rows` | Rows hidden, and how many were tool results |
| `chars` | Characters of text the model no longer sees |
| `ids` | The row ids, to look up in `messages` |

### `turn end`

One line per turn. Written by `TurnStats::log` (`koda-core/src/inference.rs`).

| Field | Meaning |
|---|---|
| `session_id` | The session |
| `outcome` | How it ended, below |
| `model_calls`, `tool_calls` | Calls that returned, and tool calls the model asked for |
| `final_text_chars` | Length of the final reply |
| `ends_with_question` | The reply ended on a question, so the model may have stopped to ask rather than finished |

| `outcome` | Meaning |
|---|---|
| `complete` | The model gave a final reply |
| `iteration_cap` | Hit the iteration cap and wasn't extended |
| `max_tokens` | Three replies in a row were cut off at the token cap |
| `loop_guard` | The loop detector stopped it after its warning was ignored |
| `aborted` | Stopped on a warning: server error, image rejection or dropped connection |
| `error` | Any other error |
| `cancelled` | The user or a signal cancelled it |
| `stopped` | Ended with no final reply and none of the above, such as a window too small for the system prompt |

### Retries and warnings

Plain-text lines worth a search:

| Line | Meaning |
|---|---|
| `Rate limited (attempt 1/5): …` | A 429, 503, 529 or similar. Backs off and retries. |
| `Network glitch (attempt 1/5): …` | A transient network error. Backs off and retries. |
| `Context overflow from provider: …` | The provider rejected the request as too long. Koda compacts and retries once. |
| `Pre-flight: context at N%, attempting auto-compact` | Compaction started |
| `Pre-flight: context at N% but the history is too small to compact` | System prompt and tools fill the window |
| `Pre-flight: context at N% but circuit breaker tripped — skipping` | 3 failures in a row. No more summary calls. |
| `Pre-flight compact skipped: …`, `Pre-flight compact failed: …` | Compaction didn't run, or its call returned an error |
| `Pre-flight: could not shrink the history: …` | The last-resort drop could not free anything |
| `Compact summary was cut off at the N-token cap …` | Retrying the summary with more room |
| `Compact reply has no usable summary: …` | Falling back to the marker |
| `Dropping N messages without a summary to fit the window` | A `dropped` compaction |
| `Microcompact: cleared N tool results, saved ~M tokens` | Old tool results overwritten after an idle gap |
| `Loop detected — injecting feedback` | First loop warning, sent to the model |

## Turning up the level

`RUST_LOG` sets the log filter. The default is `koda_core=info,koda_cli=info`.
Setting it replaces the default, so name everything you want:

```sh
# Prompts and summaries, as text
RUST_LOG=koda_core=debug,koda_cli=info koda -p "…"

# Those, plus every request body
RUST_LOG=koda_core=debug,koda_cli=info,koda_core::providers=trace koda -p "…"
```

`koda_core=debug` adds these lines, whose text runs on over the following lines
of the log:

| Line | Written |
|---|---|
| `System prompt:` | When the prompt changes, with its `system_hash` |
| `Tool definitions:` | When the tools change, with their `tools_hash` |
| `Compaction prompt:` | What the summarizer was given |
| `Compaction reply:` | What it returned, per attempt |
| `Stored compaction summary:` | What went into the history |
| `Discarded partial reply:` | A reply lost to a dropped connection |

`koda_core::providers=trace` adds a `request body` line for each request, as
sent. It is the exact answer to what the model saw.

API keys never reach the log, at any level.
`compaction_leaves_the_system_prompt_alone` in
`koda-cli/tests/gemini_stub_test.rs` runs at trace level and asserts it. Your
conversation does reach it at debug and trace, along with anything you pasted
into it.

## Triage checklist

A failed run should answer these. The default level answers all of them except
the text behind questions 3 and 4, which needs `debug` or `trace`.

| # | Question | Where to look |
|---|---|---|
| 1 | Did a call fail, retry or stop abnormally? | `model call failed`, `Rate limited`, `Network glitch`; `failed_stop=true` or `stop_reason=max_tokens` on `model call` |
| 2 | What did each call cost? | `prompt_tokens`, `completion_tokens`, `cache_read_tokens`, `thinking_tokens`, `elapsed_ms` on `model call`; totals in `run_summary.usage` |
| 3 | What did the model see on a given call? | Shape: the last `context assembled` before that `model call`. Text: the `request body` line, or `System prompt:` plus the database rows. |
| 4 | Did compaction keep the request, and what did the summarizer get and return? | `Stored compaction summary:` should carry the request. `Compaction prompt:` and `Compaction reply:` hold the text. At `info`: `branch`, `conversation_chars`, `clipped_chars`, `reply_chars` on `compaction`. |
| 5 | Was anything removed without a summary? | `history edited`; a `compaction` branch of `dropped`, `cut_off`, `empty` or `too_short`; `truncated=true`; `Microcompact: cleared` |
| 6 | Why did the context estimate move? | Compare `context assembled` lines: the `*_chars` fields, `correction` and `summary_chars`. Compare `estimated_tokens` with `context_tokens` on `model call`. |
| 7 | How did the run end? | `outcome` and `ends_with_question` on `turn end`; `turn_end` in `run_summary` |
| 8 | What was re-fetched, and at what cost? | `duplicate_read_files` and `duplicate_read_tokens` on `context assembled`, which reset when a compaction removes the reads; `tool_calls` in `run_summary` |
| 9 | Was the system prompt the same before and after compaction? | Compare `system_hash` on `purpose="turn"` lines either side of the `compaction` line |

For question 9, compaction calls send no system prompt, so they carry no hash.
The git line in the prompt is captured once at session start, so it can't move
the hash within a run.

## How compaction works

[DESIGN.md](https://github.com/lijunzh/koda/blob/main/DESIGN.md#compaction-two-layers-p2-p3)
has the reasons. In short:

- It fires at 85% of the window, measured on the estimate plus the gap the
  provider's last count showed. `/compact` runs it by hand.
- The newest 30% of the history by size stays verbatim, at least 4 rows.
- The rest is summarized by the session's own model, and the user's messages
  from that part travel beside the summary word for word, up to 4,000
  characters, newest first.
- A compaction replaces the last summary. The old one goes to the summarizer as
  input.
- Past the window with no working summary, the oldest rows are dropped for a
  marker with no model call.
- A history under a fifth of the window is left alone, because compacting it
  would free nothing.

Microcompact is a separate layer. After a 5-minute gap it clears old tool
results, with no model call.

## Reproducing without a key

The stub-server tests run the real `koda` binary against
`koda_test_utils::network::FakeLlmServer` and assert on its log with
`koda_test_utils::log::log_line`. No key, no model, no GPU.

| Test in `koda-cli/tests/` | Shows how to |
|---|---|
| `gemini_stub_test.rs` | Drive the Gemini provider: a 503, a tool call, a failed reply, a compaction |
| `anthropic_stream_failure_test.rs` | Fail an Anthropic stream after a tool call, and check nothing ran |
| `no_summary_compaction_test.rs` | Fill a small window and empty or fail the summary call |
| `context_count_test.rs` | Report token counts above the estimate, or none |
| `history_edit_log_test.rs` | Make a pass hide a row, and assert on the warning |
| `headless_interrupt_test.rs` | Send SIGINT during a model call and read the stream |

```sh
cargo test -p koda-cli --test gemini_stub_test
```

To turn a bug report into a failing test, read the report's `model call` lines
as a script: each is one response for the stub to play. Copy the closest test,
replay those responses, and assert on the line that was wrong.

```rust
//! koda-cli/tests/my_bug_test.rs
use koda_test_utils::log::{log_line, read_log};
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// One stub response per `model call` line in the report.
fn reply(request: usize) -> ResponseTemplate {
    match request {
        0 => sse(&[
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_0",
                "type": "function",
                "function": {
                    "name": "Read",
                    "arguments": json!({"file_path": "notes.txt"}).to_string(),
                },
            }]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ]),
        _ => sse(&[
            json!({"choices": [{"delta": {"content": "All done."}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ]),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_turn_finishes_after_the_read() {
    let server = FakeLlmServer::spawn().await;
    let requests = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            reply(requests.fetch_add(1, Ordering::SeqCst))
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "a note\n").unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Read notes.txt", "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", "100000"])
        .arg("--project-root")
        .arg(tmp.path())
        // Keeps the log and the database in the temp dir.
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(120), child)
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    assert!(output.status.success());

    let log = read_log(tmp.path());
    log_line(
        &log,
        "turn end",
        &[r#"outcome="complete""#, "model_calls=2", "tool_calls=1"],
    );
}
```

`log_line` returns the first line with the message and every field, or panics
with the lines it did find. Write a field as `name=value`, or `name=` when the
value varies. `server.received_requests()` returns what Koda sent, for asserting
on the request itself. A smaller `--max-context-tokens` is the way to reach
compaction, as `no_summary_compaction_test.rs` does. It also tells the two call
types apart: turns stream, summary calls don't. On Gemini the paths are
`.*:streamGenerateContent$` and `.*:generateContent$`, and the test sets
`GEMINI_API_KEY` to a dummy value.

If a bug doesn't need the binary or the wire format, `MockProvider` is lighter:
see the end-to-end tests in `koda-core/tests/`.

## What to put in a bug report

1. **The log at debug level.** Re-run with `RUST_LOG=koda_core=debug,koda_cli=info`
   if you can, then copy `~/.config/koda/logs/latest` with `cp -L`. It holds your
   prompts and the files Koda read, so check it before you post it.
2. **The `run_summary`**, for a headless run.
3. **The debug bundle**, for a TUI session: run `/debug-bundle` before you quit.
   It holds the conversation and that process's log. A headless run has no
   bundle, and one written later holds the later process's log, so send that
   run's log yourself.
4. **Which lines show the problem**, by name, such as
   `compaction branch="cut_off" attempts=2`. Say which of the questions above
   you can't answer.
5. **A failing test**, if you can write one.

File it with the [bug report template](https://github.com/lijunzh/koda/issues/new?template=bug_report.md).
