//! Shutdowner adapter for Tokio I/O streams.
//!
//! `Shutdowner<T>` is a small utility wrapper for Tokio I/O `AsyncRead` and `AsyncWrite`
//! that lets you **abort ongoing reads/writes** from the *outside* by delivering an error via a
//! `tokio::sync::oneshot::Receiver`. Once tripped, all future I/O polls on the wrapper
//! fail immediately with an `io::Error::other(...)`, making it ideal for:
//!
//! - Actively terminating a proxied connection when business rules change
//! - Implementing "kill switches" for long-lived streams
//! - Injecting failures in chaos/latency/netem test rigs
//!
//! The first error received consumes the internal receiver; subsequent polls return a
//! stable `SHUTDOWN_ERROR`. If the sender is dropped without a payload, the wrapper
//! still transitions to the shutdown state and returns `SHUTDOWN_ERROR`.
//!
//! ## At a glance
//! - Wrap any `AsyncRead`, `AsyncWrite`, or `AsyncBufRead` (e.g. `TcpStream`)
//! - Trip it by sending **any** `Box<dyn Error + Send + Sync + 'static>` through an oneshot
//! - After tripping, **all** future I/O calls error immediately
//!
//! ## Example
//! ```no_run
//! # use std::{io, time::Duration};
//! # use tokio::{net::TcpStream, sync::oneshot, time::sleep};
//! # use tokio_netem::shutdowner::{Shutdowner, SHUTDOWN_ERROR};
//!
//! # async fn demo(mut upstream: TcpStream) -> io::Result<()> {
//! let (tx, rx) = oneshot::channel();
//! let mut client = TcpStream::connect("127.0.0.1:8080").await?;
//!
//! // Wrap the client with Shutdowner. When `tx` fires, all I/O on `client_io` will error.
//! let mut client_io = Shutdowner::new(client, rx);
//!
//! // Trip the shutdown later from some control task:
//! tokio::spawn(async move {
//!     sleep(Duration::from_millis(100)).await;
//!     let _ = tx.send(io::Error::other("policy revoked").into());
//! });
//!
//! // This copy will abort as soon as the shutdown signal is received.
//! let _ = tokio::io::copy_bidirectional(&mut client_io, &mut upstream).await;
//!
//! // After trip, any further poll will return an error (SHUTDOWN_ERROR if sender was dropped).
//! // match client_io.poll_shutdown(...)
//! # Ok(()) }
//! ```
//! ## Error semantics
//! - If the oneshot delivers an error: future polls return `io::Error::other(the_error)`.
//! - If the oneshot sender is **dropped**: future polls return `io::Error::other(SHUTDOWN_ERROR)`.
//! - After the first transition, the receiver is consumed; subsequent polls keep returning
//!   a stable shutdown error without re-polling the channel.
use std::{
    error::Error,
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
};

use futures::FutureExt;
use pin_project::pin_project;
use tokio::{
    io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf},
    sync::oneshot,
};

use crate::io::ResetLinger;

/// Error returned when a `Shutdowner` has been tripped (or its sender dropped)
/// and no specific error payload was supplied.
///
/// This is used as a stable sentinel so callers can distinguish "intentional
/// shutdown" from other I/O errors if they need to.
#[derive(Debug, Copy, Clone)]
pub struct ShutdownError;

/// A convenient constant instance of [`ShutdownError`].
pub const SHUTDOWN_ERROR: ShutdownError = ShutdownError;

impl fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stream was already shut down")
    }
}

impl Error for ShutdownError {}

/// Boxed error type accepted by the shutdown channel. Send any error you want the
/// I/O calls to surface via `io::Error::other(...)`.
pub type BError = Box<dyn Error + Send + Sync + 'static>;

/// A wrapper around an async I/O type that can be **force-failed** from the outside
/// by receiving an error over a `oneshot::Receiver`.
///
/// Typical usage:
/// - Construct `(tx, rx) = oneshot::channel()`
/// - Wrap the I/O object: `let io = Shutdowner::new(inner, rx)`
/// - Keep `tx` somewhere; when you decide to kill the connection, `tx.send(err.into())`
///   (or drop `tx` to deliver a generic shutdown error)
///
/// After the first error is observed, the receiver is consumed and all subsequent
/// I/O polls fail immediately without additional channel polling.
#[pin_project]
pub struct Shutdowner<T> {
    #[pin]
    inner: T,
    /// Receiver for a single shutdown signal.
    ///
    /// Once a message is received (or the sender is dropped), this is set to `None`
    /// and the wrapper remains permanently in the shutdown state.
    err_rx: Option<oneshot::Receiver<BError>>,
}

impl<T> Shutdowner<T> {
    /// Create a new `Shutdowner` that will proxy I/O to `inner` until `err_rx`
    /// resolves or is dropped. After that, all I/O polls return an error.
    pub fn new(inner: T, err_rx: oneshot::Receiver<BError>) -> Self {
        Shutdowner {
            inner,
            err_rx: Some(err_rx),
        }
    }

    /// Poll the shutdown channel and transition to the shutdown state if a signal arrives.
    ///
    /// Returns:
    /// - `Poll::Pending` if no shutdown signal has been received yet
    /// - `Poll::Ready(Err(...))` once tripped (with payload error or `SHUTDOWN_ERROR`)
    ///
    /// After the first `Ready`, the receiver is consumed and future calls return
    /// an immediate shutdown error.
    fn poll_error(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.err_rx.is_none() {
            return Poll::Ready(Err(io::Error::other(SHUTDOWN_ERROR)));
        }

        // unwrap() is safe
        if let Poll::Ready(ret) = self.err_rx.as_mut().unwrap().poll_unpin(cx) {
            self.err_rx = None;

            return match ret {
                Ok(err) => Poll::Ready(Err(io::Error::other(err))),
                Err(_) => Poll::Ready(Err(io::Error::other(SHUTDOWN_ERROR))), // dropped
            };
        }

        // No error - continue
        Poll::Pending
    }
}

impl<T: ResetLinger> ResetLinger for Shutdowner<T> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead + Unpin> AsyncBufRead for Shutdowner<T> {
    fn poll_fill_buf(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        _ = self.poll_error(cx)?;
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Shutdowner<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        _ = self.poll_error(cx)?;
        self.project().inner.poll_read(cx, buf)
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Shutdowner<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        _ = self.poll_error(cx)?;
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        _ = self.poll_error(cx)?;
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        _ = self.poll_error(cx)?;
        self.project().inner.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        _ = self.poll_error(cx)?;
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

impl<RW: fmt::Debug> fmt::Debug for Shutdowner<RW> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fmt, io,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::duplex;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    type ShutdownPair = (oneshot::Sender<BError>, oneshot::Receiver<BError>);

    fn oneshot_pair() -> ShutdownPair {
        oneshot::channel()
    }

    #[derive(Debug)]
    struct MyErr(&'static str);
    impl fmt::Display for MyErr {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl Error for MyErr {}

    #[tokio::test]
    async fn write_passes_through_until_custom_error_then_stays_shutdown() {
        let (tx, rx) = oneshot_pair();
        let (a, mut b) = duplex(1024);

        // Wrap writer end with Shutdowner
        let mut w = Shutdowner::new(a, rx);

        // 1) Normal write works
        w.write_all(b"hello").await.unwrap();

        // Reader gets it
        let mut buf = [0u8; 5];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        // 2) Send a custom error; next I/O should error with that inner cause.
        tx.send(Box::new(MyErr("boom"))).unwrap();

        let err = w.write_all(b"!").await.expect_err("should error");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        // Ensure the inner error is our custom one
        let inner = err.get_ref().expect("inner error");
        assert_eq!(inner.to_string(), "boom");

        // 3) Once tripped, subsequent calls must immediately return SHUTDOWN_ERROR
        let err2 = w.write_all(b"!").await.expect_err("still shutdown");
        assert_eq!(err2.kind(), io::ErrorKind::Other);
        let inner2 = err2.get_ref().expect("inner error present");
        assert_eq!(inner2.to_string(), SHUTDOWN_ERROR.to_string());
    }

    #[tokio::test]
    async fn dropped_sender_yields_shutdown_error() {
        let (tx, rx) = oneshot_pair();
        drop(tx); // nobody will ever send

        let (a, _b) = duplex(64);
        let mut w = Shutdowner::new(a, rx);

        let err = w.write_all(b"x").await.expect_err("should be shutdown");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        let inner = err.get_ref().expect("inner error");
        assert_eq!(inner.to_string(), SHUTDOWN_ERROR.to_string());
    }

    #[tokio::test]
    async fn async_read_passes_then_fails_after_shutdown() {
        let (tx, rx) = oneshot_pair();
        let (mut a, b) = duplex(64);

        // Writer sends some bytes
        a.write_all(b"abc").await.unwrap();

        // Wrap reader end
        let mut r = Shutdowner::new(b, rx);

        let mut got = vec![0u8; 3];
        r.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"abc");

        // Trip the shutdown
        tx.send(Box::new(MyErr("stop"))).unwrap();

        let mut more = [0u8; 1];
        let err = r.read_exact(&mut more).await.expect_err("should error");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(err.get_ref().unwrap().to_string(), "stop");
    }

    #[tokio::test]
    async fn async_bufread_works_then_errors() {
        let (tx, rx) = oneshot_pair();
        let (mut a, b) = duplex(64);

        // Feed two lines
        a.write_all(b"line1\nline2\n").await.unwrap();

        // Use BufReader path (ensures poll_fill_buf/consume path exercised)
        let inner = BufReader::new(b);
        let mut br = Shutdowner::new(inner, rx);

        let mut s = String::new();
        br.read_line(&mut s).await.unwrap();
        assert_eq!(s, "line1\n");

        // Trip error
        tx.send(Box::new(MyErr("buf-shutdown"))).unwrap();

        s.clear();
        let err = br.read_line(&mut s).await.expect_err("should error");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(err.get_ref().unwrap().to_string(), "buf-shutdown");

        // And it stays shut down
        let err2 = br
            .read_line(&mut s)
            .await
            .expect_err("should remain shutdown after first error");
        assert_eq!(
            err2.get_ref().unwrap().to_string(),
            SHUTDOWN_ERROR.to_string()
        );
    }

    #[tokio::test]
    async fn write_vectored_and_flag_query_delegate_to_inner() {
        let (_tx, rx) = oneshot_pair();
        let (a, mut b) = duplex(1024);

        let w = Shutdowner::new(a, rx);
        // Delegation check
        assert!(w.is_write_vectored());

        // Try actual vectored write path via trait (not strictly necessary, but nice)
        let mut w = w;
        use std::io::IoSlice;
        let bufs = [IoSlice::new(b"foo"), IoSlice::new(b"bar")];
        // Use the trait method directly so we exercise poll_write_vectored
        let n = tokio::io::AsyncWrite::poll_write_vectored(
            Pin::new(&mut w),
            &mut Context::from_waker(futures::task::noop_waker_ref()),
            &bufs,
        );
        // We can't reliably assert exact number written with a raw poll, but ensure it's Ready.
        match n {
            Poll::Ready(Ok(written)) => {
                assert!(written > 0 && written <= 6, "unexpected written={written}");
            }
            other => panic!("expected Ready(Ok(..)), got {other:?}"),
        }

        // Flush through a normal call so the reader can drain what was written
        w.write_all(b"baz").await.unwrap();
        let mut out = vec![0u8; 9];
        b.read_exact(&mut out).await.unwrap();
        // We only guarantee that all bytes written are readable, not the exact
        // interleaving; but "baz" must be present.
        assert!(out.ends_with(b"baz"));
    }
}
