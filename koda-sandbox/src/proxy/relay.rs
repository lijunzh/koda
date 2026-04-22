//! Bidirectional TCP relay with idle + total timeouts (Phase 3f of #934).
//!
//! ## Why we don't just call [`tokio::io::copy_bidirectional`]
//!
//! `copy_bidirectional` is fire-and-forget: it returns when one side
//! sends EOF or when an I/O error happens. There is **no signal** to
//! cancel a tunnel that has gone idle (peer wedged, NAT silently
//! dropped, corp proxy returned 200 then never sent another byte).
//!
//! In a corp environment that's the most common pathological state we
//! see — the kernel's TCP keepalive defaults to 7200 seconds (two
//! hours!) and most middleboxes don't bother sending RST on idle. So
//! a wedged tunnel sits in `copy_bidirectional` for hours, holding a
//! task slot, an FD pair, and (worse) blocking the per-policy file
//! descriptor budget if a script churns through many such requests.
//!
//! ## What this module provides
//!
//! [`relay_with_timeouts`] is a drop-in replacement for
//! `copy_bidirectional` that adds two cancellation signals:
//!
//! * **Idle timeout** — if neither direction has produced bytes within
//!   `idle`, return `io::ErrorKind::TimedOut`. Per-direction; counted
//!   from the last successful `read()` on that direction.
//! * **Total cap** — even if both directions are actively pushing
//!   bytes, the tunnel cannot live longer than `total`. Bounds the
//!   blast radius of a runaway stream.
//!
//! Both default values ([`DEFAULT_IDLE_TIMEOUT`],
//! [`DEFAULT_TOTAL_TIMEOUT`]) are chosen so well-behaved long-poll
//! and SSE / gRPC streaming flows aren't disturbed in normal use.
//!
//! ## Implementation note
//!
//! We split each socket into reader/writer halves with [`tokio::io::split`]
//! and run two independent `copy_one_direction` tasks via
//! [`tokio::try_join!`]. The classic `select! { read … sleep … }` pattern
//! would require holding a `&mut` to both halves, which is what
//! `copy_bidirectional` already does — and it's why adding idle detection
//! upstream hasn't happened in tokio for years. Splitting first is the
//! straightforward way out.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

/// Default per-direction idle timeout. Five minutes is a deliberate
/// compromise: long-poll endpoints (Slack RTM, Server-Sent Events
/// heartbeats) typically ping every 30-60s, so 5 minutes leaves ample
/// margin while still reaping wedged tunnels well before the kernel
/// keepalive (default 7200s) would notice.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Default total-tunnel cap. One hour is enough for very large
/// downloads (a 50 GB tarball at 100 Mbps is ~70 minutes — we'd cut
/// that off, which is fine; nobody should be pulling 50 GB through a
/// dev sandbox proxy without thinking twice).
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(3600);

/// Buffer size per copy direction. 8 KiB matches `tokio::io::copy`'s
/// internal default and lines up with most NIC MTU * a small handful
/// of segments. Bigger buffers don't help throughput on loopback;
/// smaller ones hurt syscall counts.
const BUF_SIZE: usize = 8 * 1024;

/// Bidirectional TCP relay with per-direction idle + overall total
/// timeouts.
///
/// Returns `Ok((a_to_b_bytes, b_to_a_bytes))` on clean shutdown,
/// `Err(io::ErrorKind::TimedOut)` on either timeout firing, or any
/// other I/O error verbatim.
///
/// On a timeout, the *opposite* direction may still have unflushed
/// bytes in its writer's buffer. We `.shutdown()` both halves before
/// returning so callers can `drop()` the streams without leaking FDs.
pub async fn relay_with_timeouts<A, B>(
    a: A,
    b: B,
    idle: Duration,
    total: Duration,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    // tokio::try_join! short-circuits on the first error: if a→b
    // times out, b→a is dropped (its task is cancelled at the next
    // await point), which closes the reader half — exactly what we
    // want. No double-close, no half-open zombie.
    let work = async {
        tokio::try_join!(
            copy_one_direction(&mut ar, &mut bw, idle),
            copy_one_direction(&mut br, &mut aw, idle),
        )
    };

    match timeout(total, work).await {
        Ok(Ok((a_b, b_a))) => Ok((a_b, b_a)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("tunnel exceeded total cap of {total:?}"),
        )),
    }
}

/// Copy bytes from `r` to `w`, returning [`io::ErrorKind::TimedOut`]
/// if no bytes arrive within `idle` between successful reads.
///
/// Counts only **successful** reads as resets — a 0-byte EOF returns
/// `Ok(total)` so the caller's `try_join!` doesn't treat a clean
/// half-close as a failure. Mirrors the semantics of
/// [`tokio::io::copy`].
async fn copy_one_direction<R, W>(r: &mut R, w: &mut W, idle: Duration) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; BUF_SIZE];
    let mut total = 0u64;

    loop {
        let n = match timeout(idle, r.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                // Best-effort flush before reporting the timeout so the
                // peer that *was* talking sees its last frame land.
                let _ = w.shutdown().await;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("relay idle for {idle:?}"),
                ));
            }
        };
        w.write_all(&buf[..n]).await?;
        total = total.saturating_add(n as u64);
    }

    // Clean half-close: tell the writer half no more bytes are coming
    // so the peer's read() returns 0 promptly instead of waiting for
    // the next idle timeout to fire on its side.
    let _ = w.shutdown().await;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Helper: bind a 127.0.0.1:0 listener, return its addr + a future
    /// that yields the accepted server-side socket.
    async fn bound_listener() -> (std::net::SocketAddr, TcpListener) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        (addr, l)
    }

    #[tokio::test]
    async fn relays_bytes_in_both_directions_until_eof() {
        // Topology:
        //   client ──┐         ┌── server (echo)
        //            └─ relay ─┘
        // Client writes "ping", server echoes "pong-ping", client EOFs,
        // relay returns clean.
        let (server_addr, server_listener) = bound_listener().await;
        let server_task = tokio::spawn(async move {
            let (mut s, _) = server_listener.accept().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
            s.write_all(b"pong-ping").await.unwrap();
            s.shutdown().await.unwrap();
        });

        let (client_addr, client_listener) = bound_listener().await;
        let relay_task = tokio::spawn(async move {
            let (client_side, _) = client_listener.accept().await.unwrap();
            let upstream_side = TcpStream::connect(server_addr).await.unwrap();
            relay_with_timeouts(
                client_side,
                upstream_side,
                Duration::from_secs(2),
                Duration::from_secs(5),
            )
            .await
        });

        let mut client = TcpStream::connect(client_addr).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        client.shutdown().await.unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"pong-ping");

        let (a_b, b_a) = relay_task.await.unwrap().unwrap();
        assert_eq!(a_b, 4, "client→server byte count");
        assert_eq!(b_a, 9, "server→client byte count");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn idle_timeout_fires_when_both_sides_silent() {
        // Neither side ever writes after handshake. The idle timeout
        // is the only thing keeping us from hanging forever.
        let (server_addr, server_listener) = bound_listener().await;
        let _server_task = tokio::spawn(async move {
            // Accept and then sleep — never write anything.
            let (sock, _) = server_listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(sock);
        });

        let (client_addr, client_listener) = bound_listener().await;
        let start = std::time::Instant::now();
        let relay_task = tokio::spawn(async move {
            let (client_side, _) = client_listener.accept().await.unwrap();
            let upstream_side = TcpStream::connect(server_addr).await.unwrap();
            relay_with_timeouts(
                client_side,
                upstream_side,
                Duration::from_millis(150),
                Duration::from_secs(5),
            )
            .await
        });

        let _client = TcpStream::connect(client_addr).await.unwrap();
        let res = relay_task.await.unwrap();
        let elapsed = start.elapsed();

        let err = res.expect_err("expected idle timeout error");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Idle is 150 ms; allow generous slack for slow CI but assert
        // we didn't hang for the full 5 s total cap.
        assert!(
            elapsed < Duration::from_secs(2),
            "idle should fire well before total cap; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn total_timeout_fires_even_when_traffic_is_active() {
        // Server keeps trickling bytes forever — idle timeout never
        // fires because something is always arriving. Only the total
        // cap saves us.
        let (server_addr, server_listener) = bound_listener().await;
        let _server_task = tokio::spawn(async move {
            let (mut sock, _) = server_listener.accept().await.unwrap();
            // Tight loop, but with small sleeps so we don't burn CPU.
            for _ in 0..1000 {
                if sock.write_all(b".").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let (client_addr, client_listener) = bound_listener().await;
        let start = std::time::Instant::now();
        let relay_task = tokio::spawn(async move {
            let (client_side, _) = client_listener.accept().await.unwrap();
            let upstream_side = TcpStream::connect(server_addr).await.unwrap();
            relay_with_timeouts(
                client_side,
                upstream_side,
                // Idle is generous (1 s) so it can't fire — the
                // server pushes a byte every 20 ms.
                Duration::from_secs(1),
                // Total is tight (300 ms) so it MUST fire.
                Duration::from_millis(300),
            )
            .await
        });

        // Drain the client side so writes don't backpressure-stall the
        // server task before the cap fires.
        let mut client = TcpStream::connect(client_addr).await.unwrap();
        let drain = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                if client.read(&mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });

        let err = relay_task.await.unwrap().expect_err("expected total cap");
        let elapsed = start.elapsed();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(2),
            "total cap timing out of bounds: {elapsed:?}"
        );
        let _ = drain.await;
    }

    #[tokio::test]
    async fn clean_half_close_propagates_without_idle_fire() {
        // Client closes its write half but still wants to read replies.
        // copy_one_direction client→server returns Ok(0); server→client
        // direction must not be killed by an idle timeout if the server
        // is still actively replying.
        let (server_addr, server_listener) = bound_listener().await;
        let _server_task = tokio::spawn(async move {
            let (mut s, _) = server_listener.accept().await.unwrap();
            let mut buf = Vec::new();
            // Drain whatever the client sends; respond with a bunch of
            // bytes spread out over time.
            let _ = s.read_to_end(&mut buf).await;
            for _ in 0..5 {
                if s.write_all(b"chunk").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = s.shutdown().await;
        });

        let (client_addr, client_listener) = bound_listener().await;
        let relay_task = tokio::spawn(async move {
            let (client_side, _) = client_listener.accept().await.unwrap();
            let upstream_side = TcpStream::connect(server_addr).await.unwrap();
            relay_with_timeouts(
                client_side,
                upstream_side,
                Duration::from_secs(2),
                Duration::from_secs(5),
            )
            .await
        });

        let mut client = TcpStream::connect(client_addr).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.shutdown().await.unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"chunkchunkchunkchunkchunk");

        let (a_b, b_a) = relay_task.await.unwrap().unwrap();
        assert_eq!(a_b, 5);
        assert_eq!(b_a, 25);
    }
}
