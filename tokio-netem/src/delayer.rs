//! Delay adapters for Tokio I/O streams.
//!
//! This module exposes two lightweight wrappers that simulate latency in async I/O:
//! - [`DelayedReader<T, D>`] delays the **first byte of each buffered burst** read from an
//!   [`AsyncBufRead`] source. Reads within the same buffered burst are not delayed again.
//! - [`DelayedWriter<T, D>`] delays **every write** (and `shutdown`) to an [`AsyncWrite`] sink.
//!
//! ## Static configuration
//! ```no_run
//! use std::time::Duration;
//! use tokio::io::{self, AsyncReadExt, AsyncWriteExt, BufReader};
//! use tokio::net::TcpStream;
//! use tokio_netem::delayer::{DelayedReader, DelayedWriter};
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let mut stream = DelayedWriter::new(stream, Duration::from_millis(25));
//! let mut stream = DelayedReader::new(BufReader::new(stream), Duration::from_millis(10));
//!
//! stream.write_all(b"ping").await?; // incurs ~25ms before the write is forwarded
//! let mut buf = [0u8; 4];
//! stream.read_exact(&mut buf).await?; // requires an upstream echo for demonstration
//! assert_eq!(&buf, b"ping");
//! # Ok(()) }
//! ```
//!
//! ## Dynamic configuration
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use tokio::io::{self, AsyncWriteExt};
//! use tokio::net::TcpStream;
//! use tokio_netem::delayer::{DelayedWriter, DynamicDuration};
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let delay = DynamicDuration::new(Duration::from_millis(50));
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let mut writer = DelayedWriter::new(stream, delay.clone());
//!
//! writer.write_all(b"hello").await?; // suffers ~50ms delay
//! delay.set(Duration::ZERO);         // turn off delay without rebuilding the pipeline
//! writer.write_all(b"!").await?;     // forwarded immediately
//! # Ok(()) }
//! ```
//!
//! ## Under the hood
//! - Both adapters delegate reads/writes to the inner type and gate progress through a small
//!   timer state machine implemented by [`Delay`].
//! - The [`Duration`] trait abstracts over the latency source: either a plain `Duration` for
//!   a static setup or an [`Arc<DynamicDuration>`] for lock-free runtime updates.
//! - Zero delay takes a fast path that skips arming the timer entirely, keeping the hot path
//!   allocation free.
//! - [`DelayedReader`] relies on `AsyncBufRead::poll_fill_buf` to detect burst boundaries; wrap
//!   `AsyncRead` streams in [`tokio::io::BufReader`] if you need burst semantics.
use std::{
    fmt,
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time,
};

use futures::{ready, FutureExt};
use pin_project::pin_project;
use tokio::{
    io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf},
    time::{sleep, Instant, Sleep},
};

use crate::io::ResetLinger;

/// A provider of delay values for the adapters.
///
/// This trait abstracts over where the delay value comes from:
/// - Pass a plain `std::time::Duration` for a **static** delay.
/// - Pass an [`Arc<DynamicDuration>`] for a **dynamic** delay that can be changed at runtime.
///
/// A delay of `Duration::ZERO` means “disabled”.
pub trait Duration: Unpin {
    /// Returns the current delay value.
    fn duration(&self) -> time::Duration;
}

impl Duration for time::Duration {
    fn duration(&self) -> time::Duration {
        *self
    }
}

/// A lock-free, shareable delay knob backed by an atomic number of nanoseconds.
///
/// Use this when you want to adjust latency at runtime (e.g., via an admin endpoint).
/// Cloning the `Arc<DynamicDuration>` is cheap; all holders observe updates immediately.
///
/// **Concurrency:** reads use `Acquire` and writes use `Release` ordering. The value is
/// stored as a `u64` representing nanoseconds.
///
/// **Ranges/precision:** values are truncated to `u64` nanoseconds on set.
///
/// # Example
/// ```no_run
/// # use std::sync::Arc;
/// # use std::time;
/// # use tokio_netem::delayer::{Duration, DynamicDuration};
///
/// let d: Arc<DynamicDuration> = DynamicDuration::new(time::Duration::from_millis(25));
/// d.set(time::Duration::from_millis(100)); // update later
/// assert_eq!(d.duration(), time::Duration::from_millis(100));
/// ```
#[derive(Debug, Default)]
pub struct DynamicDuration {
    duration: AtomicU64,
}

impl DynamicDuration {
    /// Creates a new dynamic delay initialized to `duration`.
    pub fn new(duration: time::Duration) -> Arc<Self> {
        let nanos = duration.as_nanos() as u64;

        let duration = Self {
            duration: AtomicU64::new(nanos),
        };

        Arc::new(duration)
    }

    /// Atomically sets the delay to `duration`.
    ///
    /// A value of `Duration::ZERO` disables delay (fast path).
    pub fn set(&self, duration: time::Duration) {
        let nanos = duration.as_nanos() as u64;
        self.duration.store(nanos, Ordering::Release);
    }
}

impl Duration for DynamicDuration {
    fn duration(&self) -> time::Duration {
        time::Duration::from_nanos(self.duration.load(Ordering::Acquire))
    }
}

impl Duration for Arc<DynamicDuration> {
    fn duration(&self) -> time::Duration {
        time::Duration::from_nanos(self.duration.load(Ordering::Acquire))
    }
}

#[derive(Debug)]
enum Action {
    BeforeDelay,
    AfterDelay,
}

#[derive(Default, Debug)]
enum State {
    #[default]
    Idle,
    Delayed,
}

/// Internal helper encapsulating the sleep state machine.
///
/// Not part of the public API. It ensures:
/// - Zero-delay takes the fast path without arming a timer.
/// - `Delayed` → `Idle` transitions only after the sleep fires.
struct Delay<D: Duration> {
    state: State,
    sleep: Pin<Box<Sleep>>,
    delay_duration: D,
}

impl<D: Duration> Delay<D> {
    fn new(delay_duration: D) -> Self {
        Self {
            state: State::default(),
            sleep: Box::pin(sleep(time::Duration::ZERO)),
            delay_duration,
        }
    }

    /// If a delay is configured, arm the timer and return `true`.
    /// Otherwise return `false` (fast path).
    fn maybe_delay(&mut self) -> bool {
        match self.state {
            State::Idle => {
                let duration = self.delay_duration.duration();

                if duration.is_zero() {
                    // shortcut for no delay
                    return false;
                }

                self.state = State::Delayed;
                self.sleep.as_mut().reset(Instant::now() + duration);
                true
            }
            State::Delayed => {
                unreachable!("trying to delay when state is already Delayed")
            }
        }
    }
}

impl<D: Duration> Future for Delay<D> {
    type Output = Action;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            State::Idle => Poll::Ready(Action::BeforeDelay),
            State::Delayed => {
                ready!(self.sleep.as_mut().poll(cx));
                self.state = State::Idle;
                Poll::Ready(Action::AfterDelay)
            }
        }
    }
}

/// An `AsyncRead` adapter that delays the **first read of each new burst** from an `AsyncBufRead`.
///
/// While the inner buffer still has unread bytes (as reported by `poll_fill_buf`),
/// `DelayedReader` will **not** re-apply the delay; it only sleeps again once the
/// buffered data has been fully consumed and another burst arrives.
///
/// Writes are forwarded unchanged (pass-through `AsyncWrite` when `T: AsyncWrite`).
///
/// ### When to use
/// - Simulate one-way propagation latency on the inbound path.
/// - Introduce latency only at burst boundaries, not per chunk, to emulate “first byte” delays.
///
/// ### Behavior
/// - Delay applies on `poll_read` when the internal buffer transitions from **empty** to **non-empty**.
/// - Requires `T: AsyncBufRead` so it can inspect the buffer (wrap a reader in `BufReader` if needed).
/// - A zero delay value yields a fast path without arming timers.
///
/// ### See also
/// - [`DelayedWriter`] for write-side delay.
///
/// ### Example
/// ```no_run
/// # use tokio::io::{duplex, AsyncWriteExt, AsyncReadExt, BufReader};
/// # use std::time::Duration;
/// # use tokio_netem::delayer::{DelayedReader, DynamicDuration};
/// # #[tokio::main(flavor="current_thread")]
/// # async fn main() {
///     let (mut w, r) = duplex(64);
///     let br = BufReader::new(r);
///     let d = DynamicDuration::new(Duration::from_millis(20));
///     let mut dr = DelayedReader::new(br, d);
///
///     w.write_all(b"abc").await.unwrap();
///
///     let mut buf = [0u8; 3];
///     dr.read_exact(&mut buf).await.unwrap(); // incurs ~20ms once
/// # }
/// ```
#[pin_project]
pub struct DelayedReader<T, D: Duration> {
    #[pin]
    inner: T,
    delay: Delay<D>,
    buf_empty: bool,
}

impl<T, D: Duration> DelayedReader<T, D> {
    /// Creates a new [`DelayedReader`] around `inner` with the provided delay provider.
    ///
    /// Pass a plain `std::time::Duration` for a static delay, or an `Arc<DynamicDuration>` for
    /// a dynamic, runtime-adjustable delay.
    pub fn new(inner: T, delay_duration: D) -> Self {
        Self {
            inner,
            delay: Delay::new(delay_duration),
            buf_empty: true,
        }
    }
}

/// Forwards `set_reset_linger` into the inner I/O when supported.
impl<R: ResetLinger, D: Duration> ResetLinger for DelayedReader<R, D> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<R: AsyncBufRead, D: Duration> AsyncBufRead for DelayedReader<R, D> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<R: AsyncBufRead, D: Duration> AsyncRead for DelayedReader<R, D> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut this = self.project();

        loop {
            match ready!(this.delay.poll_unpin(cx)) {
                Action::BeforeDelay => {
                    let (amt, empty) = {
                        // waiting for new data to arrive
                        let rem = ready!(this.inner.as_mut().poll_fill_buf(cx))?;

                        if *this.buf_empty && this.delay.maybe_delay() {
                            continue; // sleep in delay.poll()
                        }

                        let amt = rem.len().min(buf.remaining());
                        buf.put_slice(&rem[..amt]);
                        (amt, rem.len() == amt)
                    };

                    this.inner.consume(amt);
                    *this.buf_empty = empty; // don't sleep until buf is empty
                    return Poll::Ready(Ok(()));
                }
                Action::AfterDelay => {
                    let (amt, empty) = {
                        // read data from the buffer
                        let Poll::Ready(rem) = this.inner.as_mut().poll_fill_buf(cx)? else {
                            unreachable!("buffer can't be empty");
                        };
                        let amt = rem.len().min(buf.remaining());
                        buf.put_slice(&rem[..amt]);
                        (amt, rem.len() == amt)
                    };

                    this.inner.consume(amt); // decrement internal buffer
                    *this.buf_empty = empty; // don't sleep until buf is empty

                    return Poll::Ready(Ok(()));
                }
            };
        }
    }
}

impl<W: AsyncWrite, D: Duration> AsyncWrite for DelayedReader<W, D> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

impl<T: fmt::Debug, D: Duration> fmt::Debug for DelayedReader<T, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// An `AsyncWrite` adapter that delays **every write** (and `shutdown`) by the configured duration.
///
/// Reads are forwarded unchanged (pass-through `AsyncRead` when `T: AsyncRead`).
///
/// ### Behavior
/// - Delay applies before `poll_write`, `poll_write_vectored`, and `poll_shutdown`.
/// - `poll_flush` is **not** delayed (pass-through).
/// - A zero delay value yields a fast path (no timer).
///
/// ### Example
/// ```no_run
/// # use tokio::io::{duplex, AsyncWriteExt};
/// # use std::time::Duration;
/// # use tokio_netem::delayer::{DelayedWriter, DynamicDuration};
/// # #[tokio::main(flavor="current_thread")]
/// # async fn main() {
///     let (w, _r) = duplex(64);
///     let d = DynamicDuration::new(Duration::from_millis(10));
///     let mut w = DelayedWriter::new(w, d);
///     w.write_all(b"X").await.unwrap(); // delayed ~10ms
///     w.flush().await.unwrap();         // not delayed
/// # }
/// ```
#[pin_project]
pub struct DelayedWriter<T, D: Duration> {
    #[pin]
    inner: T,
    delay: Delay<D>,
}

impl<T, D: Duration> DelayedWriter<T, D> {
    /// Creates a new [`DelayedWriter`] around `inner` with the provided delay provider.
    ///
    /// Pass a plain `std::time::Duration` for a static delay, or an `Arc<DynamicDuration>` for
    /// a dynamic, runtime-adjustable delay.
    pub fn new(inner: T, delay_duration: D) -> Self {
        Self {
            inner,
            delay: Delay::new(delay_duration),
        }
    }

    fn poll_with_write_delay<R>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        f: impl FnOnce(&mut Context<'_>, Pin<&mut T>) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let this = self.project();

        loop {
            match ready!(this.delay.poll_unpin(cx)) {
                Action::BeforeDelay => {
                    if !this.delay.maybe_delay() {
                        // shortcut for 0 delay
                        return f(cx, this.inner);
                    }

                    // poll write_delay sleep
                    continue;
                }
                Action::AfterDelay => return f(cx, this.inner),
            }
        }
    }
}

impl<R: ResetLinger, D: Duration> ResetLinger for DelayedWriter<R, D> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead, D: Duration> AsyncBufRead for DelayedWriter<T, D> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<R: AsyncRead, D: Duration> AsyncRead for DelayedWriter<R, D> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl<W: AsyncWrite, D: Duration> AsyncWrite for DelayedWriter<W, D> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_with_write_delay(cx, |cx, inner| inner.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_with_write_delay(cx, |cx, inner| inner.poll_shutdown(cx))
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_with_write_delay(cx, |cx, inner| inner.poll_write_vectored(cx, bufs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::time;

    #[test]
    fn duration_set_get_roundtrip() {
        let d = DynamicDuration::default();
        assert!(d.duration().is_zero());

        d.set(time::Duration::from_millis(150));
        assert_eq!(d.duration(), time::Duration::from_millis(150));
        assert!(!d.duration().is_zero());

        d.set(time::Duration::ZERO);
        assert_eq!(d.duration().as_nanos(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_writer_waits_before_each_write() {
        let (w, mut r) = duplex(64);
        let d = DynamicDuration::new(time::Duration::from_millis(100));
        let w = DelayedWriter::new(w, d.clone());

        // First write should not appear until 100ms
        let write = tokio::spawn(async move {
            let mut dw2 = w;
            dw2.write_all(b"hi").await.unwrap();
            dw2.flush().await.unwrap();
        });

        // Before advancing time, reader should see nothing
        let try_read = tokio::time::timeout(time::Duration::from_millis(1), r.read_u8()).await;
        assert!(try_read.is_err(), "unexpectedly read before delay elapsed");

        time::sleep(time::Duration::from_millis(100)).await;
        // Now bytes should arrive
        let mut buf = [0u8; 2];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");

        write.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_reader_only_on_new_bursts() {
        let (mut w, r) = duplex(64);
        let br = BufReader::new(r);
        let d = DynamicDuration::new(time::Duration::from_millis(50));
        let mut dr = DelayedReader::new(br, d.clone());

        // Write a first burst
        tokio::spawn(async move {
            let _ = w.write_all(b"abcdef").await;
        });

        // First read delayed
        let mut buf = [0u8; 3];
        let pending =
            tokio::time::timeout(time::Duration::from_millis(1), dr.read_exact(&mut buf)).await;
        assert!(pending.is_err());

        // Advance time -> first chunk delivered
        time::sleep(time::Duration::from_millis(50)).await;
        dr.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abc");

        // Subsequent reads from the same buffered burst should not be delayed
        let mut buf2 = [0u8; 3];
        // This should complete immediately without additional sleep
        dr.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"def");

        // Now a new burst later should incur delay again
        let (mut w2, r2) = duplex(64);
        let br2 = BufReader::new(r2);
        let mut dr2 = DelayedReader::new(br2, d.clone());

        tokio::spawn(async move {
            let _ = w2.write_all(b"ZZ").await;
        });

        let mut buf3 = [0u8; 2];
        let pending2 =
            tokio::time::timeout(time::Duration::from_millis(1), dr2.read_exact(&mut buf3)).await;
        assert!(pending2.is_err());
        time::sleep(time::Duration::from_millis(50)).await;
        dr2.read_exact(&mut buf3).await.unwrap();
        assert_eq!(&buf3, b"ZZ");
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_writer_zero_delay_fast_path() {
        let (w, mut r) = duplex(64);
        let d = DynamicDuration::new(time::Duration::ZERO);
        let mut dw = DelayedWriter::new(w, d);

        // Should not require advancing time
        dw.write_all(b"ok").await.unwrap();
        dw.flush().await.unwrap();
        let mut buf = [0u8; 2];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_writer_shutdown_is_delayed() {
        let (w, _r) = duplex(64);
        let d = DynamicDuration::new(time::Duration::from_millis(40));
        let mut dw = DelayedWriter::new(w, d);

        // shutdown should be delayed like a write
        let mut shutdown = Box::pin(futures::future::poll_fn(|cx| {
            Pin::new(&mut dw).poll_shutdown(cx)
        }));

        // Should still be pending before time advance
        let early = tokio::time::timeout(time::Duration::from_millis(1), &mut shutdown).await;
        assert!(early.is_err(), "shutdown completed before delay");

        time::sleep(time::Duration::from_millis(40)).await;
        shutdown.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_writer_vectored_write_is_delayed() {
        use futures::future::poll_fn;
        use std::io::IoSlice;

        let (w, mut r) = duplex(64);
        let d = DynamicDuration::new(time::Duration::from_millis(25));
        let mut dw = DelayedWriter::new(w, d);

        let a = IoSlice::new(b"hello ");
        let b = IoSlice::new(b"vectored");

        // call poll_write_vectored directly to exercise that path
        let mut fut = Box::pin(poll_fn(|cx| {
            Pin::new(&mut dw).poll_write_vectored(cx, &[a, b])
        }));

        // Not ready until delay elapsed
        let early = tokio::time::timeout(time::Duration::from_millis(1), &mut fut).await;
        assert!(early.is_err(), "vectored write completed before delay");

        time::sleep(time::Duration::from_millis(25)).await;
        let n = fut.await.unwrap();
        assert_eq!(n, b"hello vectored".len());

        // Data actually arrived
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello vectored");
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_writer_dynamic_toggle_runtime() {
        let (dw, mut r) = duplex(64);
        let d = DynamicDuration::new(time::Duration::from_millis(30));
        let mut dw = DelayedWriter::new(dw, d.clone());

        // First write delayed
        let write1 = tokio::spawn(async move {
            dw.write_all(b"A").await.unwrap();
            dw.flush().await.unwrap();
            dw
        });

        let early = tokio::time::timeout(time::Duration::from_millis(1), r.read_u8()).await;
        assert!(early.is_err());
        time::sleep(time::Duration::from_millis(30)).await;
        assert_eq!(r.read_u8().await.unwrap(), b'A');
        let mut dw = write1.await.unwrap();

        // Toggle to zero => next write immediate
        d.set(time::Duration::ZERO);
        dw.write_all(b"B").await.unwrap();
        dw.flush().await.unwrap();
        assert_eq!(r.read_u8().await.unwrap(), b'B');
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_writer_flush_is_not_delayed() {
        let (w, _r) = duplex(64);
        let d = DynamicDuration::new(time::Duration::from_millis(50));
        let mut dw = DelayedWriter::new(w, d);

        // Flush should be pass-through (no delay)
        tokio::time::timeout(time::Duration::from_millis(1), dw.flush())
            .await
            .expect("flush should not be delayed")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_reader_zero_delay_fast_path() {
        let (mut w, r) = duplex(64);
        let br = BufReader::new(r);
        let d = DynamicDuration::new(time::Duration::ZERO);
        let mut dr = DelayedReader::new(br, d);

        w.write_all(b"OK").await.unwrap();
        w.flush().await.unwrap();

        // Should complete without advancing time
        let mut buf = [0u8; 2];
        tokio::time::timeout(time::Duration::from_millis(1), dr.read_exact(&mut buf))
            .await
            .expect("read should be immediate")
            .unwrap();
        assert_eq!(&buf, b"OK");
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_reader_dynamic_toggle_runtime() {
        let (mut w, r) = duplex(64);
        let br = BufReader::new(r);
        let d = DynamicDuration::new(time::Duration::ZERO);
        let mut dr = DelayedReader::new(br, d.clone());

        // First burst with zero delay
        w.write_all(b"11").await.unwrap();
        w.flush().await.unwrap();
        let mut buf = [0u8; 2];
        tokio::time::timeout(time::Duration::from_millis(1), dr.read_exact(&mut buf))
            .await
            .expect("read should be immediate")
            .unwrap();
        assert_eq!(&buf, b"11");

        // Toggle delay on; next burst should be delayed
        d.set(time::Duration::from_millis(40));
        w.write_all(b"22").await.unwrap();
        w.flush().await.unwrap();

        let pending =
            tokio::time::timeout(time::Duration::from_millis(1), dr.read_exact(&mut buf)).await;
        assert!(pending.is_err(), "read completed before delay");
        time::sleep(time::Duration::from_millis(40)).await;
        dr.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"22");
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_reader_forwards_writes_without_delay() {
        // DelayedReader implements AsyncWrite by pass-through; ensure no delay is applied.
        let (w, mut r) = duplex(64);
        let d = DynamicDuration::new(time::Duration::from_millis(100));
        let mut drw = DelayedReader::new(w, d); // wrap the writer end

        // Should not require advancing time despite non-zero delay knob
        drw.write_all(b"pw").await.unwrap();
        drw.flush().await.unwrap();

        let mut buf = [0u8; 2];
        tokio::time::timeout(time::Duration::from_millis(1), r.read_exact(&mut buf))
            .await
            .expect("write path should not be delayed")
            .unwrap();
        assert_eq!(&buf, b"pw");
    }
}
