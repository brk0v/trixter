//! Throttling utilities for Tokio I/O `AsyncRead` and `AsyncWrite` streams.
//!
//! Two adapters expose a byte-per-second knob backed by the [`Rate`] trait:
//! - [`ThrottledWriter`] limits outbound writes to an [`AsyncWrite`] sink.
//! - [`ThrottledReader`] limits reads from an [`AsyncBufRead`] source by controlling how much of the
//!   inner buffer is consumed on each poll.
//!
//! Rates are expressed in **bytes per second**. Supplying `0` disables throttling entirely.
//!
//! ## Static configuration
//! ```no_run
//! use tokio::io::{self, AsyncWriteExt};
//! use tokio::net::TcpStream;
//! use tokio_netem::throttler::ThrottledWriter;
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let mut writer = ThrottledWriter::new(stream, 32usize); // 32 B/s throttle, static value
//!
//! writer.write_all(b"abcdefgh").await?;
//! writer.flush().await?; // ensure paced bytes are forwarded
//! # Ok(()) }
//! ```
//!
//! ## Dynamic configuration
//! ```no_run
//! use std::sync::Arc;
//! use tokio::io::{self, AsyncReadExt, AsyncWriteExt, BufReader};
//! use tokio::net::TcpStream;
//! use tokio_netem::throttler::{DynamicRate, ThrottledReader, ThrottledWriter};
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let rate: Arc<DynamicRate> = DynamicRate::new(0); // start unlimited
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let (reader_half, writer_half) = stream.into_split();
//! let mut writer = ThrottledWriter::new(writer_half, rate.clone());
//! let mut reader = ThrottledReader::new(BufReader::new(reader_half), rate.clone());
//!
//! writer.write_all(b"ping").await?; // no throttling yet
//! rate.set(4);                       // drop to 4 B/s without reconfiguring the pipeline
//! writer.write_all(b"pong").await?; // paced writes + reads share the same knob
//! let mut buf = [0u8; 8];
//! reader.read_exact(&mut buf).await?; // requires the remote peer to send at least 8 bytes
//! # Ok(()) }
//! ```
//!
//! ## Under the hood
//! - Both adapters share a leaky-bucket token counter backed by [`tokio::time::Instant`] and
//!   `Sleep`. After idle periods, the budget is capped to roughly one second of tokens to limit
//!   bursts.
//! - `ThrottledReader` requires an [`AsyncBufRead`] inner type so it can consume only part of the
//!   buffered data when the budget is tight; pair it with `tokio::io::BufReader` if necessary.
//! - A rate of `0` short-circuits the throttler and bypasses the token logic entirely (fast path).
use std::{
    fmt, io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{ready, Context, Poll},
    time::Duration,
};

use futures::FutureExt;
use pin_project::pin_project;
use smallvec::SmallVec;
use tokio::{
    io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf},
    time::{sleep, Instant, Sleep},
};

use crate::io::ResetLinger;

/// Inline `IoSlice` capacity for zero-allocation vectored writes in the common path.
///
/// When throttling vectored writes we build a trimmed view of the caller's buffers
/// constrained by the current token grant. Up to `INLINE_IOVEC` entries are stored inline
/// (no heap allocation) via a `SmallVec`; larger inputs may allocate.
const INLINE_IOVEC: usize = 16;

/// A pluggable **bytes-per-second** rate knob.
///
/// Implementors report a current rate in bytes per second. A rate of `0` disables
/// throttling. This abstraction lets you pass either a fixed value (`usize`) or a
/// shareable handle such as [`DynamicRate`] or `Arc<DynamicRate>`.
pub trait Rate: Unpin {
    /// Return the current rate in **bytes per second**. `0` means **unlimited**.
    fn rate(&self) -> usize;

    /// Convenience check for `self.rate() == 0`.
    fn is_unlimited(&self) -> bool;
}

impl Rate for usize {
    fn rate(&self) -> usize {
        *self
    }

    fn is_unlimited(&self) -> bool {
        *self == 0
    }
}

/// A cheap, lock-free, shareable rate knob (`bytes/sec`) you can update at runtime.
///
/// Internally uses an `AtomicUsize`; clones are cheap and can be shared across many
/// throttlers. Setting the rate to `0` disables throttling.
#[derive(Debug, Default)]
pub struct DynamicRate {
    size: AtomicUsize,
}

impl DynamicRate {
    /// Create a new handle with the given rate in **bytes per second**.
    ///
    /// A value of `0` disables throttling.
    pub fn new(size: usize) -> Arc<Self> {
        Arc::new(Self {
            size: AtomicUsize::new(size),
        })
    }

    /// Update the rate in **bytes per second**. `0` disables throttling.
    pub fn set(&self, size: usize) {
        self.size.store(size, Ordering::Release);
    }
}

impl Rate for DynamicRate {
    fn rate(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    fn is_unlimited(&self) -> bool {
        self.size.load(Ordering::Acquire) == 0
    }
}

impl Rate for Arc<DynamicRate> {
    fn rate(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    fn is_unlimited(&self) -> bool {
        self.size.load(Ordering::Acquire) == 0
    }
}

struct LeakyBucket<R> {
    rate: R,
    budget: usize,
    last_update: Instant,
    sleep: Pin<Box<Sleep>>,
    sleeping: bool,
}

impl<R: Rate> LeakyBucket<R> {
    #[inline]
    fn new(rate: R) -> Self {
        let budget = rate.rate();
        Self {
            rate,
            budget,
            last_update: Instant::now(),
            sleep: Box::pin(sleep(Duration::ZERO)),
            sleeping: false,
        }
    }

    #[inline]
    fn update_budget(&mut self) {
        let now = Instant::now();
        let rate = self.rate.rate() as u128;
        let since = now.duration_since(self.last_update).as_nanos();
        let added = since * rate / 1_000_000_000;

        if added > 0 {
            self.last_update = now;
        } else {
            // Protect against very small durations
            return;
        }

        let new_budget = (self.budget as u128).saturating_add(added).min(rate); // allow 1s burst
        self.budget = new_budget as usize;
    }

    fn poll_acquire(mut self: Pin<&mut Self>, cx: &mut Context<'_>, want: usize) -> Poll<usize> {
        if self.sleeping {
            ready!(self.sleep.poll_unpin(cx));
            self.sleeping = false;
        }

        if want == 0 {
            return Poll::Ready(0);
        }

        let rate = self.rate.rate();
        if rate == 0 {
            return Poll::Ready(want);
        }

        loop {
            self.update_budget();

            let grant = want.min(self.budget);
            if grant > 0 {
                return Poll::Ready(grant);
            }

            // Sleep for up to 100ms, or long enough to accrue ~1 KiB—whichever is smaller.
            // This reduces syscall count while keeping latency reasonable.
            let ms_for_1kib = (1024 * 1000 / rate as u64).max(1);
            let wake_up = Duration::from_millis(100u64.min(ms_for_1kib));
            self.sleep.as_mut().reset(Instant::now() + wake_up);
            self.sleeping = true;

            ready!(self.sleep.poll_unpin(cx));
        }
    }

    /// Deduct `used` tokens after actual I/O.
    #[inline]
    fn consume(&mut self, used: usize) {
        self.budget = self.budget.saturating_sub(used);
    }
}

/// A read-side throttler for buffered sources.
///
/// Wraps any `T: AsyncBufRead` and limits how many bytes are yielded to callers
/// per unit time. Throttling is applied by controlling how much of the inner
/// buffer is copied into the caller’s `ReadBuf` per poll.
///
/// If you need to throttle an unbuffered `AsyncRead`, place a `BufReader`
/// upstream and wrap that:
///
/// ```no_run
/// use tokio::io::{self, duplex, BufReader};
/// use tokio_netem::throttler::ThrottledReader;
///
/// # #[tokio::main]
/// # async fn main() -> io::Result<()> {
/// let (_w, r) = duplex(128);
/// let br = BufReader::new(r);
/// let _tr = ThrottledReader::new(br, 32usize); // 32 B/s
/// # Ok(()) }
/// ```
#[pin_project]
pub struct ThrottledReader<T, R> {
    #[pin]
    inner: T,

    #[pin]
    lb: LeakyBucket<R>,
}

impl<T, R: Rate> ThrottledReader<T, R> {
    /// Wrap `inner` and enforce the given rate.
    ///
    /// You may pass a fixed `usize` or a dynamic handle like `Arc<DynamicRate>`.
    pub fn new(inner: T, rate: R) -> Self {
        Self {
            inner,
            lb: LeakyBucket::new(rate),
        }
    }
}

impl<T: ResetLinger, R: Rate> ResetLinger for ThrottledReader<T, R> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead, R: Rate> AsyncBufRead for ThrottledReader<T, R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<T: AsyncBufRead, R: Rate> AsyncRead for ThrottledReader<T, R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut this = self.project();

        if this.lb.rate.is_unlimited() {
            return this.inner.poll_read(cx, buf);
        }

        // 1. waiting for buffer
        let rem = ready!(this.inner.as_mut().poll_fill_buf(cx))?;

        // 2. ask for capacity
        let grant = ready!(this.lb.as_mut().poll_acquire(cx, rem.len()));

        // 3. get the min
        let grant = grant.min(buf.remaining());
        if grant == 0 {
            // buf is 0
            return Poll::Ready(Ok(()));
        }

        buf.put_slice(&rem[..grant]);
        this.inner.consume(grant);
        this.lb.as_mut().consume(grant);

        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite, R: Rate> AsyncWrite for ThrottledReader<W, R> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().inner.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

impl<RW: fmt::Debug, R> fmt::Debug for ThrottledReader<RW, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// A write-side throttler for sinks.
///
/// Wraps any `W: AsyncWrite` and limits outbound throughput. Throttling is applied
/// on each `poll_write` / `poll_write_vectored` by acquiring tokens before
/// forwarding to the inner writer.
///
/// `flush` and `shutdown` are pass-through.
///
/// Vectored writes use a small inline `SmallVec<[IoSlice; INLINE_IOVEC]>` to
/// construct a truncated view up to the granted byte budget, avoiding heap
/// allocation in the common case.
///
/// # Examples
/// ```no_run
/// use tokio::io::{self, duplex, AsyncWriteExt};
/// use tokio_netem::throttler::ThrottledWriter;
///
/// # #[tokio::main]
/// # async fn main() -> io::Result<()> {
/// let (mut w, _r) = duplex(128);
/// let mut tw = ThrottledWriter::new(&mut w, 8usize); // 8 B/s
/// tw.write_all(b"hello world").await?;
/// tw.flush().await?;
/// # Ok(()) }
/// ```
#[pin_project]
pub struct ThrottledWriter<T, R> {
    #[pin]
    inner: T,

    #[pin]
    lb: LeakyBucket<R>,
}

impl<T, R: Rate> ThrottledWriter<T, R> {
    pub fn new(inner: T, rate: R) -> Self {
        Self {
            inner,
            lb: LeakyBucket::new(rate),
        }
    }
}

impl<T: ResetLinger, R: Rate> ResetLinger for ThrottledWriter<T, R> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead, R: Rate> AsyncBufRead for ThrottledWriter<T, R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<T: AsyncRead, R: Rate> AsyncRead for ThrottledWriter<T, R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl<W: AsyncWrite, R: Rate> AsyncWrite for ThrottledWriter<W, R> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.project();

        if this.lb.rate.is_unlimited() {
            return this.inner.poll_write(cx, buf);
        }

        let want = buf.len();

        let grant = ready!(this.lb.as_mut().poll_acquire(cx, want));
        let grant = grant.min(buf.len());
        if grant == 0 {
            // buf is zero
            return Poll::Ready(Ok(0));
        }

        match this.inner.poll_write(cx, &buf[..grant]) {
            Poll::Ready(Ok(n)) => {
                this.lb.as_mut().consume(n);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
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
        let mut this = self.project();

        if this.lb.rate.is_unlimited() {
            return this.inner.poll_write_vectored(cx, bufs);
        }

        let total: usize = bufs.iter().map(|b| b.len()).sum();
        let grant = ready!(this.lb.as_mut().poll_acquire(cx, total));
        if grant == 0 {
            // buf is zero
            return Poll::Ready(Ok(0));
        }

        // Build a limited view without heap allocs in the common case.
        let mut remaining = grant;
        let mut slices: SmallVec<[io::IoSlice<'_>; INLINE_IOVEC]> = SmallVec::new();
        for s in bufs {
            if remaining == 0 {
                break;
            }
            let take = s.len().min(remaining);
            // Safety: `IoSlice` is just a view; `s` lives for the call.
            slices.push(io::IoSlice::new(&s[..take]));
            remaining -= take;
        }

        match this.inner.poll_write_vectored(cx, &slices) {
            Poll::Ready(Ok(n)) => {
                this.lb.consume(n);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }
}

impl<RW: fmt::Debug, R> fmt::Debug for ThrottledWriter<RW, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::time::{self, Duration, Instant};

    #[tokio::test(start_paused = true)]
    async fn write_passes_through_when_rate_zero() {
        let (mut w, mut r) = duplex(64);
        let mut tw = ThrottledWriter::new(&mut w, 0usize);
        tw.write_all(b"hello").await.unwrap();
        tw.flush().await.unwrap();

        let mut buf = vec![0u8; 5];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test(start_paused = true)]
    async fn write_is_rate_limited_over_time() {
        let (mut w, mut r) = duplex(1024);
        // 10 B/s rate
        let mut tw = ThrottledWriter::new(&mut w, 10usize);

        let data = vec![b'a'; 30];
        let start = Instant::now();
        let write_fut = tw.write_all(&data);
        tokio::pin!(write_fut);

        // Should not complete immediately (we only advance virtual time by 10ms here).
        tokio::select! {
            _ = &mut write_fut => panic!("write completed immediately despite throttling"),
            _ = time::sleep(Duration::from_millis(10)) => {}
        }

        // Advance virtual time by 2 seconds: initial budget gives 10 bytes instantly, remaining 20 need ~2s.
        time::sleep(Duration::from_secs(2)).await;

        // Now the throttled write must have completed.
        write_fut.await.unwrap();
        tw.flush().await.unwrap();

        // All bytes should have been forwarded to the reader side.
        let mut buf = vec![0u8; data.len()];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, data);

        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(2),
            "elapsed {:?} < 2s",
            elapsed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn write_vectored_respects_rate_and_reports_bytes() {
        let (mut w, mut r) = duplex(128);
        let mut tw = ThrottledWriter::new(&mut w, 8usize); // 8 B/s

        let a = io::IoSlice::new(b"hello ");
        let b = io::IoSlice::new(b"world");
        // First vectored write should be limited to <= 8 bytes
        let n = tw.write_vectored(&[a, b]).await.unwrap();
        assert!(n <= 8);
        tw.flush().await.unwrap();

        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf[..], &b"hello world"[..n]);
    }

    #[tokio::test(start_paused = true)]
    async fn reader_throttles_bufread_stream() {
        let (mut w, r) = duplex(256);
        let br = BufReader::new(r);
        let mut tr = ThrottledReader::new(br, 16usize); // 16 B/s

        // Producer: write 48 bytes at once
        let data = vec![b'x'; 48];
        tokio::spawn(async move {
            let _ = w.write_all(&data).await;
        });

        let start = Instant::now();
        let mut out = Vec::new();
        // Read all 48 bytes
        let mut buf = [0u8; 64];
        loop {
            let n = tr.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() >= 48 {
                break;
            }
        }
        assert_eq!(out.len(), 48);
        let elapsed = start.elapsed();
        // Expect at least 2 seconds: initial 16 immediate, remaining 32 need ~2s.
        assert!(
            elapsed >= Duration::from_secs(2),
            "elapsed {:?} < 2s",
            elapsed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dynamic_rate_runtime_update() {
        let (mut w, mut r) = duplex(256);
        let rate = DynamicRate::new(0); // unlimited initially
        let mut tw = ThrottledWriter::new(&mut w, rate.clone());

        // Unlimited
        tw.write_all(b"abc").await.unwrap();
        tw.flush().await.unwrap();
        let mut buf = [0u8; 3];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abc");

        // Now limit to 1 B/s and attempt 3 bytes
        rate.set(1);
        let start = Instant::now();
        tw.write_all(b"xyz").await.unwrap();
        tw.flush().await.unwrap();
        let mut buf2 = [0u8; 3];
        r.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"xyz");
        assert!(start.elapsed() >= Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn write_zero_len_returns_immediately() {
        let (mut w, _r) = duplex(64);
        let mut tw = ThrottledWriter::new(&mut w, 5usize);
        let start = Instant::now();
        let n = tw.write(&[]).await.unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "should not sleep for empty writes"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn write_vectored_empty_slice_returns_zero_immediately() {
        let (mut w, _r) = duplex(64);
        let mut tw = ThrottledWriter::new(&mut w, 5usize);
        let start = Instant::now();
        let n = tw.write_vectored(&[]).await.unwrap();
        assert_eq!(n, 0);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn burst_cap_is_at_most_one_second_of_budget() {
        let (mut w, mut r) = duplex(1024);
        let mut tw = ThrottledWriter::new(&mut w, 10usize); // 10 B/s

        // Let time pass a lot — budget must cap to 1s worth (10 bytes)
        time::sleep(Duration::from_secs(10)).await;

        let data = vec![b'z'; 100];
        let n = tw.write(&data).await.unwrap();
        assert!(
            (1..=10).contains(&n),
            "first write should be limited to <= 10 bytes, got {}",
            n
        );
        tw.flush().await.unwrap();

        let mut got = vec![0u8; n];
        r.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, &data[..n]);
    }

    #[tokio::test(start_paused = true)]
    async fn reader_unlimited_is_pass_through_without_sleep() {
        let (mut w, r) = duplex(256);
        let data = vec![42u8; 50];

        // Producer writes then drops (to deliver EOF)
        tokio::spawn({
            let data = data.clone();
            async move {
                let _ = w.write_all(&data).await;
                // drop w at scope end to close
            }
        });

        let br = BufReader::new(r);
        let mut tr = ThrottledReader::new(br, 0usize); // unlimited
        let start = Instant::now();
        let mut out = Vec::new();
        tr.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, data);
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "unlimited path must not sleep"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reader_small_buffer_consumes_initial_budget_without_sleep() {
        let (mut w, r) = duplex(128);
        let br = BufReader::new(r);
        let mut tr = ThrottledReader::new(br, 10usize); // 10 B/s
        let src = *b"abcdefghij"; // 10 bytes

        tokio::spawn(async move {
            let _ = w.write_all(&src).await;
        });

        let start = Instant::now();
        let mut tmp = [0u8; 5];
        let n1 = tr.read(&mut tmp).await.unwrap();
        assert_eq!(n1, 5);
        assert_eq!(&tmp, b"abcde");

        let n2 = tr.read(&mut tmp).await.unwrap();
        assert_eq!(n2, 5);
        assert_eq!(&tmp, b"fghij");

        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "both reads should fit initial 1s budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn write_vectored_many_slices_truncates_to_grant_without_heap_in_common_path() {
        let (mut w, mut r) = duplex(256);
        let mut tw = ThrottledWriter::new(&mut w, 10usize); // 10 B/s

        // Build 20 single-byte slices (exceeds INLINE_IOVEC=16)
        let src = [b'a'; 20];
        let mut slices: Vec<io::IoSlice<'_>> = Vec::with_capacity(src.len());
        for i in 0..src.len() {
            slices.push(io::IoSlice::new(&src[i..i + 1]));
        }

        let n = tw.write_vectored(&slices).await.unwrap();
        assert!(
            (1..=10).contains(&n),
            "should write no more than the initial 1s budget (10), got {}",
            n
        );
        tw.flush().await.unwrap();

        let mut got = vec![0u8; n];
        r.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, &src[..n]);
    }

    // Helper wrapper to verify ResetLinger delegation.
    struct RlWrapper<W> {
        inner: W,
        hits: Arc<AtomicUsize>,
    }

    impl<W> RlWrapper<W> {
        fn new(inner: W, hits: Arc<AtomicUsize>) -> Self {
            Self { inner, hits }
        }
    }

    impl<W> ResetLinger for RlWrapper<W> {
        fn set_reset_linger(&mut self) -> io::Result<()> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for RlWrapper<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }

        fn is_write_vectored(&self) -> bool {
            self.inner.is_write_vectored()
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn set_reset_linger_delegates_to_inner() {
        let (w, _r) = duplex(32);
        let hits = Arc::new(AtomicUsize::new(0));
        let wrapped = RlWrapper::new(w, hits.clone());

        let mut tw = ThrottledWriter::new(wrapped, 0usize);
        tw.set_reset_linger().unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
