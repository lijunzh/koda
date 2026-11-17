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
//! - **A client that reads slowly:** messages wait in a queue until it
//!   reads them; none is dropped, and the engine never waits. Codex's stdio
//!   does the same: its engine's event queue is unbounded and only the
//!   forwarder to stdout waits. Goose and Pi keep everything too. Koda's
//!   engine reports events synchronously, so waiting for the reader would
//!   block engine threads, and a turn `turn/interrupt` could no longer reach.
//!   The queue grows only while the client lags, and EOF or a broken stdout
//!   ends it.
//! - **A message over 64 MiB** (Codex's WebSocket limit, tungstenite's
//!   default) gets an error and ends the connection, rather than being read
//!   into memory whole.
//! - **EOF on stdin, a broken stdout or the caller's stop** ends the
//!   connection: running turns stop, open server requests are cancelled, what
//!   is queued is written while the pipe takes it, and [`serve`] returns.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use koda_core::config::KodaConfig;
use koda_protocol::rpc::{self, ErrorObject, ErrorResponse, Message, RequestId, codes};
use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinSet;

use crate::ConfigFile;
use crate::host::HostCommand;
use crate::in_process::{HostStart, RequestError, launch, stop_host};

/// How long, once the connection has ended and the host has stopped, what
/// is still queued may take to write.
const DRAIN: Duration = Duration::from_secs(5);

/// What [`serve`] needs: where the host keeps its state and works. The
/// client says who it is in `initialize`.
pub struct ServeArgs {
    /// The config file, as [`crate::in_process::HostStart::config_file`].
    pub config_file: ConfigFile,
    /// The project sessions work in.
    pub project_root: PathBuf,
    /// Koda's folder, as [`crate::in_process::HostStart::home`].
    pub home: PathBuf,
}

/// Serve one client: read its messages from `input`, write Koda's to
/// `output`, until `input` ends, `output` breaks or `stop` completes. New
/// sessions start from the config file, as `koda_client::InProcessClient::start`'s do.
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
                match message.to_json() {
                    Ok(line) => outbox.push(line),
                    Err(e) => tracing::error!("koda serve: a message didn't encode: {e}"),
                }
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
                read = read_message(&mut lines, &mut line) => read,
                () = &mut stop => break,
                () = outbox.broken.notified() => break,
            };
            match read {
                Ok(Read::End) => break,
                Ok(Read::Line) => {}
                Ok(Read::TooLong) => {
                    outbox.push(too_long_line());
                    break;
                }
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
            outbox.push(error);
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
                    outbox.push(line);
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
pub(crate) fn parse(line: &[u8]) -> Result<Message, String> {
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

pub(crate) fn error_line(id: Option<RequestId>, error: ErrorObject) -> serde_json::Result<String> {
    serde_json::to_string(&ErrorResponse::new(id, error))
}

/// The largest message Koda reads, as Codex's WebSocket transport takes
/// (tungstenite's default): a pasted image in base64 fits, and a client that
/// never sends a newline can't grow the server without end.
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// What [`read_message`] read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Read {
    /// The other end hung up.
    End,
    /// One message, in the buffer.
    Line,
    /// A message over [`MAX_MESSAGE_BYTES`]; the connection should end.
    TooLong,
}

/// Read one message, a line, into `line`, stopping at
/// [`MAX_MESSAGE_BYTES`].
pub(crate) async fn read_message<R: AsyncBufRead + Unpin>(
    input: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<Read> {
    read_capped(input, line, MAX_MESSAGE_BYTES).await
}

/// [`read_message`] with a limit of `max` bytes before the newline.
async fn read_capped<R: AsyncBufRead + Unpin>(
    input: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Read> {
    let cap = max as u64 + 1;
    let read = (&mut *input).take(cap).read_until(b'\n', line).await?;
    Ok(if read == 0 {
        Read::End
    } else if line.len() as u64 == cap && line.last() != Some(&b'\n') {
        Read::TooLong
    } else {
        Read::Line
    })
}

/// The error a client gets for a message over [`MAX_MESSAGE_BYTES`].
pub(crate) fn too_long_line() -> String {
    error_line(
        None,
        ErrorObject::new(
            codes::INVALID_REQUEST,
            format!(
                "a message is larger than {} MiB; the connection is closed",
                MAX_MESSAGE_BYTES >> 20
            ),
        ),
    )
    .unwrap_or_default()
}

/// Lines waiting for the client, oldest first.
#[derive(Default)]
pub(crate) struct Outbox {
    queue: Mutex<VecDeque<String>>,
    /// Something was queued, or the outbox closed.
    ready: Notify,
    /// Nothing more will be queued.
    closed: AtomicBool,
    /// Stdout broke, or the client fell too far behind: stop reading.
    pub(crate) broken: Notify,
    /// How many lines may wait before the client counts as gone; `None`
    /// never gives up (stdio, whose one client can't be replaced).
    limit: Option<usize>,
    /// The client fell [`Outbox::limit`] lines behind: what waited is
    /// dropped and nothing more is queued.
    overflowed: AtomicBool,
}

impl Outbox {
    /// An outbox whose client counts as gone once `limit` lines wait, as
    /// Codex drops a socket client whose queue fills; the client reattaches
    /// and reads the session again.
    pub(crate) fn bounded(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a line. With a limit, a full queue drops everything waiting and
    /// ends the connection instead.
    pub(crate) fn push(&self, line: String) {
        let mut queue = self.lock();
        if self.overflowed.load(Ordering::Acquire) {
            return;
        }
        if self.limit.is_some_and(|limit| queue.len() >= limit) {
            self.overflowed.store(true, Ordering::Release);
            queue.clear();
            drop(queue);
            self.broken.notify_one();
            self.ready.notify_one();
            return;
        }
        queue.push_back(line);
        drop(queue);
        self.ready.notify_one();
    }

    /// Whether the client fell too far behind and was let go.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_one();
    }

    /// The next line, or `None` once the outbox is closed and empty.
    async fn next(&self) -> Option<String> {
        loop {
            if let Some(line) = self.lock().pop_front() {
                return Some(line);
            }
            if self.closed.load(Ordering::Acquire) || self.overflowed() {
                return None;
            }
            self.ready.notified().await;
        }
    }
}

/// Write queued lines to `output` until the outbox closes or `output`
/// breaks.
pub(crate) async fn write_lines<W: AsyncWrite + Unpin>(outbox: Arc<Outbox>, mut output: W) {
    while let Some(mut line) = outbox.next().await {
        line.push('\n');
        let written = async {
            output.write_all(line.as_bytes()).await?;
            output.flush().await
        };
        if let Err(e) = written.await {
            tracing::warn!("koda serve: writing to the client failed: {e}");
            outbox.broken.notify_one();
            return;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_message_up_to_the_limit_is_read_and_one_past_it_is_not() {
        let mut line = Vec::new();
        let mut input: &[u8] = b"12345678\n123456789\n";
        assert_eq!(
            read_capped(&mut input, &mut line, 8).await.unwrap(),
            Read::Line
        );
        assert_eq!(line, b"12345678\n");
        line.clear();
        assert_eq!(
            read_capped(&mut input, &mut line, 8).await.unwrap(),
            Read::TooLong
        );
        line.clear();
        let mut last: &[u8] = b"1234";
        assert_eq!(
            read_capped(&mut last, &mut line, 8).await.unwrap(),
            Read::Line
        );
        line.clear();
        assert_eq!(
            read_capped(&mut last, &mut line, 8).await.unwrap(),
            Read::End
        );
        assert!(too_long_line().contains("64 MiB"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bounded_outbox_lets_its_client_go_once_full() {
        let outbox = Outbox::bounded(2);
        outbox.push("a".into());
        outbox.push("b".into());
        assert!(!outbox.overflowed());
        outbox.push("c".into());
        assert!(outbox.overflowed());
        // What waited is dropped, nothing more is queued, and the reader is
        // told to stop.
        outbox.push("d".into());
        assert_eq!(outbox.next().await, None);
        tokio::time::timeout(std::time::Duration::from_secs(1), outbox.broken.notified())
            .await
            .expect("the connection is told to stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unbounded_outbox_keeps_everything() {
        let outbox = Outbox::default();
        for i in 0..10_000 {
            outbox.push(i.to_string());
        }
        assert!(!outbox.overflowed());
        assert_eq!(outbox.next().await.as_deref(), Some("0"));
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
}
