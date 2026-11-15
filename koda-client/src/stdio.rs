//! `koda serve --stdio`: Koda's host for a client on the other end of a
//! pipe, one JSON-RPC message per line each way, as `codex app-server`
//! serves stdio.
//!
//! The client sends `initialize` first, then requests; `initialize` is
//! answered before the next line is read, and the host answers each other
//! request on its own task, so a slow one never holds up the rest, and the
//! client's answers to server requests (approvals, questions, the loop cap)
//! are taken while a turn runs.
//!
//! - **A line that isn't a JSON-RPC message** gets JSON-RPC's own error
//!   (`-32700` for one that isn't JSON or UTF-8, `-32600` for one that isn't
//!   a request), with a null id where none can be read. Codex logs such a
//!   line and drops it; Koda keeps `"jsonrpc"` so that standard libraries
//!   work, and the standard says to answer. Batches aren't taken.
//! - **A client that reads slowly:** messages wait in a queue. Once
//!   [`BACKLOG`] are waiting, streaming deltas are dropped, as Codex's
//!   in-process server drops notifications when its consumer falls behind;
//!   `item/completed` carries an item's whole text, so nothing is lost.
//!   Every other message is kept, so the queue grows only with the work
//!   done, never with the text streamed. Koda's engine reports events
//!   synchronously, so waiting for the reader, as Codex's stdio does, would
//!   block engine threads, and a turn `turn/interrupt` could no longer reach.
//! - **EOF on stdin, a broken stdout or the caller's stop** ends the
//!   connection: running turns stop, open server requests are cancelled, what
//!   is queued is written while the pipe takes it, and [`serve`] returns.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_protocol::rpc::{self, ErrorObject, ErrorResponse, Message, RequestId, codes};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinSet;

use crate::events::Outgoing;
use crate::host::HostCommand;
use crate::{ConfigFile, HostStart, RequestError, launch, stop_host};

/// How many messages may wait for the client before deltas are dropped:
/// Codex's queue size.
pub const BACKLOG: usize = 128;

/// How long, once the connection has ended and the host has stopped, what
/// is still queued may take to write.
const DRAIN: Duration = Duration::from_secs(5);

/// What [`serve`] needs: where the host keeps its state and works. The
/// client says who it is in `initialize`.
pub struct ServeArgs {
    /// The config file, as [`crate::StartArgs::config_file`].
    pub config_file: ConfigFile,
    /// The project sessions work in.
    pub project_root: PathBuf,
    /// Koda's folder, as [`crate::StartArgs::home`].
    pub home: PathBuf,
}

/// Serve one client: read its messages from `input`, write Koda's to
/// `output`, until `input` ends, `output` breaks or `stop` completes. New
/// sessions start from the config file, as [`crate::InProcessClient::start`]'s do.
/// A config file or database that can't be opened stops it before anything
/// is read.
pub async fn serve<R, W>(
    args: ServeArgs,
    input: R,
    output: W,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), RequestError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_on(args, None, input, output, stop).await
}

/// [`serve`] with new sessions starting from `config`, for tests that need
/// settings no key gives (a scripted server's address).
#[doc(hidden)]
pub async fn serve_with_config<R, W>(
    args: ServeArgs,
    config: KodaConfig,
    input: R,
    output: W,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), RequestError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_on(args, Some(config), input, output, stop).await
}

async fn serve_on<R, W>(
    args: ServeArgs,
    config: Option<KodaConfig>,
    input: R,
    output: W,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), RequestError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let launched = launch(
        HostStart {
            config_file: args.config_file,
            project_root: args.project_root,
            home: args.home,
        },
        config,
        None,
    )
    .await?;
    let commands = launched.requests.commands.clone();
    let outbox = Arc::new(Outbox::default());

    let mut events = launched.events;
    let pump = {
        let outbox = Arc::clone(&outbox);
        tokio::spawn(async move {
            while let Some(message) = events.recv().await {
                outbox.push_event(message);
            }
        })
    };
    let writer = tokio::spawn(write_lines(Arc::clone(&outbox), output));

    let mut answering = JoinSet::new();
    {
        let mut lines = BufReader::new(input);
        let mut line = Vec::new();
        tokio::pin!(stop);
        loop {
            line.clear();
            let read = tokio::select! {
                read = lines.read_until(b'\n', &mut line) => read,
                () = &mut stop => break,
                () = outbox.broken.notified() => break,
            };
            match read {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("koda serve: reading stdin failed: {e}");
                    break;
                }
            }
            if !take_line(&line, &commands, &outbox, &mut answering).await {
                break;
            }
            // Requests that have been answered.
            while answering.try_join_next().is_some() {}
        }
    }

    stop_host(&commands, launched.host).await;
    // The host's requests are answered or dropped with it.
    let _ = tokio::time::timeout(DRAIN, async {
        while answering.join_next().await.is_some() {}
    })
    .await;
    drop(commands);
    // Everything the stopping sessions sent is queued once the pump has
    // seen the last of them.
    let _ = tokio::time::timeout(DRAIN, pump).await;
    outbox.close();
    if tokio::time::timeout(DRAIN, writer).await.is_err() {
        tracing::warn!("koda serve: stdout didn't take the last messages in time");
    }
    Ok(())
}

/// Handle one line from the client. `false` once the host has stopped.
async fn take_line(
    line: &[u8],
    commands: &tokio::sync::mpsc::Sender<HostCommand>,
    outbox: &Arc<Outbox>,
    answering: &mut JoinSet<()>,
) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return true;
    }
    let message = match parse(line) {
        Ok(message) => message,
        Err(error) => {
            outbox.push_answer(error);
            return true;
        }
    };
    let sent = match message {
        Message::Request(request) => {
            let id = request.id.clone();
            // `initialize` is answered before the next line is read, so a
            // client may send its first requests right behind it, as
            // Codex's server takes them.
            let initialize = request.method
                == <koda_protocol::methods::Initialize as koda_protocol::Method>::NAME;
            let (reply, answer) = oneshot::channel();
            let sent = commands
                .send(HostCommand::Request { request, reply })
                .await
                .is_ok();
            let outbox = Arc::clone(outbox);
            let respond = async move {
                let line = match answer.await {
                    Ok(Ok(result)) => serde_json::to_string(&rpc::Response::new(id, result)),
                    Ok(Err(error)) => error_line(Some(id), error),
                    Err(_) => error_line(
                        Some(id),
                        ErrorObject::new(codes::INTERNAL_ERROR, "Koda's host has stopped"),
                    ),
                };
                if let Ok(line) = line {
                    outbox.push_answer(line);
                }
            };
            if initialize {
                respond.await;
            } else {
                answering.spawn(respond);
            }
            sent
        }
        Message::Response(response) => commands
            .send(HostCommand::Response {
                id: response.id,
                result: Ok(response.result),
            })
            .await
            .is_ok(),
        Message::Error(ErrorResponse {
            id: Some(id),
            error,
            ..
        }) => commands
            .send(HostCommand::Response {
                id,
                result: Err(error),
            })
            .await
            .is_ok(),
        Message::Error(ErrorResponse {
            id: None, error, ..
        }) => {
            tracing::warn!("koda serve: the client sent an error with no id: {error}");
            true
        }
        // Koda defines no notifications from the client; nothing answers one.
        Message::Notification(notification) => {
            tracing::debug!(
                "koda serve: ignored the client's notification {}",
                notification.method
            );
            true
        }
    };
    if !sent {
        tracing::warn!("koda serve: Koda's host has stopped");
    }
    sent
}

/// One line as a message, or the error line to answer it with.
fn parse(line: &[u8]) -> Result<Message, String> {
    let fail = |id: Option<RequestId>, code, message: String| {
        error_line(id, ErrorObject::new(code, message)).unwrap_or_default()
    };
    let text = std::str::from_utf8(line)
        .map_err(|e| fail(None, codes::PARSE_ERROR, format!("not UTF-8: {e}")))?;
    let value: Value = serde_json::from_str(text)
        .map_err(|e| fail(None, codes::PARSE_ERROR, format!("not JSON: {e}")))?;
    if value.is_array() {
        return Err(fail(
            None,
            codes::INVALID_REQUEST,
            "batches aren't supported: send one message per line".to_string(),
        ));
    }
    // The id, where one can be read, so the client knows which request
    // was refused.
    let id = value
        .get("id")
        .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
    serde_json::from_value(value).map_err(|e| {
        fail(
            id,
            codes::INVALID_REQUEST,
            format!(
                "not a JSON-RPC 2.0 request, response or notification \
                 (`\"jsonrpc\": \"2.0\"` with `id` and `method`, `result` or `error`): {e}"
            ),
        )
    })
}

fn error_line(id: Option<RequestId>, error: ErrorObject) -> serde_json::Result<String> {
    serde_json::to_string(&ErrorResponse::new(id, error))
}

/// Lines waiting for the client, oldest first.
#[derive(Default)]
struct Outbox {
    queue: Mutex<VecDeque<String>>,
    /// Something was queued, or the outbox closed.
    ready: Notify,
    /// Nothing more will be queued.
    closed: AtomicBool,
    /// Stdout broke: stop reading.
    broken: Notify,
    /// Deltas dropped since the client last kept up.
    dropped: AtomicU64,
}

impl Outbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a notification or server request, dropping a delta when the
    /// client is [`BACKLOG`] behind.
    fn push_event(&self, message: Outgoing) {
        let line = match message.to_json() {
            Ok(line) => line,
            Err(e) => {
                tracing::error!("koda serve: a message didn't encode: {e}");
                return;
            }
        };
        {
            let mut queue = self.lock();
            if message.is_delta() && queue.len() >= BACKLOG {
                if self.dropped.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!(
                        "koda serve: the client is {BACKLOG} messages behind; \
                         dropping deltas until it catches up"
                    );
                }
                return;
            }
            queue.push_back(line);
        }
        self.ready.notify_one();
    }

    /// Queue a response; never dropped.
    fn push_answer(&self, line: String) {
        self.lock().push_back(line);
        self.ready.notify_one();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_one();
    }

    /// The next line, or `None` once the outbox is closed and empty.
    async fn next(&self) -> Option<String> {
        loop {
            if let Some(line) = self.lock().pop_front() {
                return Some(line);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            self.ready.notified().await;
        }
    }
}

/// Write queued lines to `output` until the outbox closes or `output`
/// breaks.
async fn write_lines<W: AsyncWrite + Unpin>(outbox: Arc<Outbox>, mut output: W) {
    while let Some(mut line) = outbox.next().await {
        line.push('\n');
        let written = async {
            output.write_all(line.as_bytes()).await?;
            output.flush().await
        };
        if let Err(e) = written.await {
            tracing::warn!("koda serve: writing stdout failed: {e}");
            outbox.broken.notify_one();
            return;
        }
        if outbox.lock().is_empty() {
            let dropped = outbox.dropped.swap(0, Ordering::Relaxed);
            if dropped > 0 {
                tracing::info!("koda serve: the client caught up; {dropped} deltas were dropped");
            }
        }
    }
    let _ = output.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_of(line: &str) -> Value {
        let line = parse(line.as_bytes()).unwrap_err();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn a_line_that_isnt_json_gets_a_parse_error() {
        let error = error_of("{not json");
        assert_eq!(error["error"]["code"], codes::PARSE_ERROR);
        assert_eq!(error["id"], Value::Null);
        assert_eq!(error["jsonrpc"], "2.0");
    }

    #[test]
    fn a_line_that_isnt_utf8_gets_a_parse_error() {
        let line = parse(b"{\"a\": \"\xff\"}").unwrap_err();
        let error: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(error["error"]["code"], codes::PARSE_ERROR);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("UTF-8")
        );
    }

    #[test]
    fn a_message_that_isnt_json_rpc_is_refused_with_its_id() {
        let error = error_of(r#"{"id": 7, "method": "state/read"}"#);
        assert_eq!(error["error"]["code"], codes::INVALID_REQUEST);
        assert_eq!(error["id"], 7);
        let error = error_of(r#"[{"jsonrpc": "2.0", "id": 1, "method": "state/read"}]"#);
        assert_eq!(error["error"]["code"], codes::INVALID_REQUEST);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("batches")
        );
    }

    #[test]
    fn messages_of_each_kind_are_read() {
        let request = parse(br#"{"jsonrpc": "2.0", "id": "a", "method": "state/read"}"#).unwrap();
        assert!(matches!(request, Message::Request(_)));
        let answer = parse(br#"{"jsonrpc": "2.0", "id": 3, "result": {}}"#).unwrap();
        assert!(matches!(answer, Message::Response(_)));
        let refusal =
            parse(br#"{"jsonrpc": "2.0", "id": 3, "error": {"code": 1, "message": "no"}}"#)
                .unwrap();
        assert!(matches!(refusal, Message::Error(_)));
    }

    fn delta() -> Outgoing {
        use koda_protocol::ServerNotification;
        use koda_protocol::item::TextDeltaNotification;
        Outgoing::Notification(ServerNotification::AgentMessageDelta(
            TextDeltaNotification {
                session_id: "s".into(),
                turn_id: "t".into(),
                item_id: "i".into(),
                delta: "x".into(),
            },
        ))
    }

    #[test]
    fn a_full_outbox_drops_deltas_and_keeps_answers() {
        let outbox = Outbox::default();
        for _ in 0..BACKLOG + 10 {
            outbox.push_event(delta());
        }
        assert_eq!(outbox.lock().len(), BACKLOG);
        assert_eq!(outbox.dropped.load(Ordering::Relaxed), 10);
        outbox.push_answer("answer".into());
        assert_eq!(outbox.lock().len(), BACKLOG + 1);
    }
}
