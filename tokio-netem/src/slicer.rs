//! Slice-and-flush writer adapter for Tokio I/O streams.
//!
//! `SlicedWriter` wraps any [`AsyncWrite`] and forces writes to be chunked into
//! fixed-size segments. After each segment is written, it immediately calls
//! [`poll_flush`](AsyncWrite::poll_flush) on the inner writer before reporting
//! completion, encouraging downstream processing at segment boundaries.
//!
//! Typical scenarios:
//! - explore MTU-like behavior by preventing large coalesced writes
//! - test application-level framing/backpressure when paired with buffered writers
//! - build chaos rigs that can toggle slice sizes at runtime
//!
//! ## Static configuration
//! ```no_run
//! use tokio::io::{self, AsyncWriteExt};
//! use tokio::net::TcpStream;
//! use tokio_netem::slicer::SlicedWriter;
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let mut writer = SlicedWriter::new(stream, 4usize); // 4-byte segments, static
//!
//! writer.write_all(b"abcdef").await?; // forwarded as 4-byte + 2-byte slices with flush between
//! writer.write_all(b"ghij").await?;
//! # Ok(()) }
//! ```
//!
//! ## Dynamic configuration
//! ```no_run
//! use std::sync::Arc;
//! use tokio::io::{self, AsyncWriteExt, BufWriter};
//! use tokio::net::TcpStream;
//! use tokio_netem::slicer::{DynamicSize, SlicedWriter};
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let knob: Arc<DynamicSize> = DynamicSize::new(8);
//! let mut writer = SlicedWriter::new(BufWriter::new(stream), knob.clone());
//!
//! writer.write_all(b"12345678").await?; // forwarded in 8-byte flush-before-return chunks
//! knob.set(2);                            // shrink slices without rebuilding the pipeline
//! writer.write_all(b"zzzz").await?;       // now flushed every 2 bytes
//! # Ok(()) }
//! ```
//!
//! A slice size of **`0`** disables the adapter, making it behave like a pass-through writer.
use std::{
    fmt, io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{ready, Context, Poll},
};

use pin_project::pin_project;
use smallvec::SmallVec;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};

use crate::io::ResetLinger;

/// Inline [`IoSlice`] capacity for the temporary stack buffer used by
/// [`SlicedWriter::poll_write_vectored`]. Increasing this may reduce heap
/// traffic for workloads with many small iovecs, at the cost of stack usage.
const INLINE_IOVEC: usize = 16;

/// Strategy for determining the current slice size.
///
/// Implementors provide the *maximum number of bytes* that a single call to
/// [`AsyncWrite::poll_write`] / [`AsyncWrite::poll_write_vectored`] is allowed
/// to forward to the inner writer **before** a flush is forced. A size of
/// **`0`** disables slicing and turns the wrapper into a pass-through.
///
/// This trait is intentionally minimal so that:
/// - a plain `usize` can be used in single-threaded/static configurations;
/// - a shareable, lock-free handle like [`DynamicSize`] can be used when the
///   slice must be adjustable at runtime from other tasks/threads.
pub trait Size: Unpin {
    /// Return the current slice budget in bytes.
    fn size(&self) -> usize;

    /// Convenience predicate; equivalent to `self.size() == 0`.
    fn is_zero(&self) -> bool;
}

impl Size for usize {
    fn size(&self) -> usize {
        *self
    }

    fn is_zero(&self) -> bool {
        *self == 0
    }
}

/// Lock-free, shareable slice-size knob.
///
/// [`DynamicSize`] stores a single `usize` in an atomic and exposes it via the
/// [`Size`] trait. Cloning uses [`Arc`], making it cheap to pass the same knob
/// to multiple `SlicedWriter`s or to admin/control paths (e.g., HTTP handlers).
///
/// *Memory ordering.*
/// - Writers call [`Size::size`] using `Acquire`.
/// - Updates call [`DynamicSize::set`] using `Release`.
///   This provides a simple *happens-before* relation between updaters and readers.
///
/// A size of **`0`** disables slicing and turns the wrapper into a pass-through.
#[derive(Debug, Default)]
pub struct DynamicSize {
    size: AtomicUsize,
}

impl DynamicSize {
    /// Create a new dynamic size handle initialized to `size`.
    ///
    /// # Examples
    /// ```
    /// use std::sync::Arc;
    /// use tokio_netem::slicer::{DynamicSize, Size};
    ///
    /// let knob: Arc<DynamicSize> = DynamicSize::new(8);
    /// assert_eq!(knob.size(), 8);
    /// ```
    pub fn new(size: usize) -> Arc<Self> {
        Arc::new(Self {
            size: AtomicUsize::new(size),
        })
    }

    /// Update the slice size seen by all users of this handle.
    ///
    /// Passing `0` disables slicing (pass-through).
    pub fn set(&self, size: usize) {
        self.size.store(size, Ordering::Release);
    }
}

impl Size for DynamicSize {
    fn size(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    fn is_zero(&self) -> bool {
        self.size.load(Ordering::Acquire) == 0
    }
}

impl Size for Arc<DynamicSize> {
    fn size(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    fn is_zero(&self) -> bool {
        self.size.load(Ordering::Acquire) == 0
    }
}

#[derive(Default, Debug)]
enum Phase {
    /// Normal write path: issue a (possibly limited) write to the inner writer.
    #[default]
    Writing,
    /// A write just completed; store the number of bytes reported written and
    /// ensure `flush` completes before returning `Ready(Ok(written))`.
    Flushing(usize),
}

#[pin_project]
pub struct SlicedWriter<T, S> {
    #[pin]
    inner: T,
    size: S,
    phase: Phase,
}

/// A writer adapter that slices each write into fixed-size segments and flushes
/// between segments.
///
/// See the [module-level](self) documentation for an overview and examples.
///
/// # Type Parameters
/// * `T` — inner type that implements [`AsyncWrite`] (and optionally [`AsyncBufRead`]/[`AsyncRead`]).
/// * `S` — a [`Size`] provider (e.g., `usize`, [`DynamicSize`], or `Arc<DynamicSize>`).
///
/// # Behavior
/// - On each call to `poll_write`/`poll_write_vectored`, at most `S::size()` bytes are forwarded to
///   the inner writer. The exact number returned is whatever the inner writer reports.
/// - Immediately after a successful write, `poll_flush` is invoked and must complete before
///   the wrapper returns `Ready(Ok(n))`.
/// - If the size is `0`, calls are forwarded unchanged (no slicing, no forced flush).
///
/// # Vectored writes
/// The adapter builds a limited list of [`IoSlice`]s whose total length does not exceed
/// the current slice size, then forwards a single vectored write followed by a flush.
///
/// # AsyncBufRead / AsyncRead passthrough
/// If `T` implements [`AsyncBufRead`] and/or [`AsyncRead`], those methods are forwarded unchanged.
impl<T, S> SlicedWriter<T, S> {
    /// Wrap an inner IO object with a shared slice-size handle.
    ///
    /// The provided `size` can be adjusted at runtime from other tasks/threads
    /// if it is a shareable type (e.g., [`Arc<DynamicSize>`]).
    ///
    /// # Examples
    /// ```no_run
    /// use tokio::io::{duplex, AsyncWriteExt, BufWriter};
    /// use tokio_netem::slicer::{SlicedWriter, DynamicSize};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// let (w, mut r) = duplex(64);
    /// let knob = DynamicSize::new(4);
    /// let mut sw = SlicedWriter::new(BufWriter::new(w), knob.clone());
    ///
    /// let n = sw.write(b"abcdef").await?;
    /// assert_eq!(n, 4);
    /// # Ok(()) }
    /// ```
    pub fn new(inner: T, size: S) -> Self {
        Self {
            inner,
            size,
            phase: Default::default(),
        }
    }
}

impl<R: ResetLinger, S> ResetLinger for SlicedWriter<R, S> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead, S> AsyncBufRead for SlicedWriter<T, S> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<R: AsyncRead, S> AsyncRead for SlicedWriter<R, S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl<W: AsyncWrite, S: Size> AsyncWrite for SlicedWriter<W, S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.as_mut().project();

        if this.size.is_zero() || buf.is_empty() {
            return this.inner.poll_write(cx, buf);
        }

        let size = this.size.size();

        loop {
            match this.phase {
                Phase::Writing => {
                    let chunk_len = buf.len().min(size);
                    if chunk_len == 0 {
                        return Poll::Ready(Ok(0));
                    }

                    let written = ready!(this.inner.as_mut().poll_write(cx, &buf[..chunk_len])?);
                    *this.phase = Phase::Flushing(written);
                }
                Phase::Flushing(size) => {
                    let size = *size;

                    ready!(this.inner.as_mut().poll_flush(cx)?);
                    *this.phase = Phase::Writing;

                    return Poll::Ready(Ok(size));
                }
            }
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
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.as_mut().project();

        if this.size.is_zero() || bufs.is_empty() {
            return this.inner.as_mut().poll_write_vectored(cx, bufs);
        }

        loop {
            match this.phase {
                Phase::Writing => {
                    let mut remaining = this.size.size();

                    let mut limited: SmallVec<[io::IoSlice<'_>; INLINE_IOVEC]> = SmallVec::new();

                    for s in bufs {
                        if remaining == 0 {
                            break;
                        }
                        let slice = if s.len() <= remaining {
                            remaining -= s.len();
                            s.as_ref()
                        } else {
                            let taken = remaining;
                            remaining = 0;
                            &s.as_ref()[..taken]
                        };
                        limited.push(io::IoSlice::new(slice));
                    }

                    let written = ready!(this.inner.as_mut().poll_write_vectored(cx, &limited)?);
                    *this.phase = Phase::Flushing(written);
                }

                Phase::Flushing(size) => {
                    let size = *size;

                    ready!(this.inner.as_mut().poll_flush(cx)?);
                    *this.phase = Phase::Writing;

                    return Poll::Ready(Ok(size));
                }
            }
        }
    }
}

impl<T: fmt::Debug, S> fmt::Debug for SlicedWriter<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, BufWriter};

    #[tokio::test]
    async fn unlimited_size_pass_through() {
        let (mut w, mut r) = duplex(64);
        let mut sw = SlicedWriter::new(&mut w, 0usize);
        sw.write_all(b"abc").await.unwrap();
        sw.flush().await.unwrap();
        let mut buf = [0u8; 3];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abc");
    }

    #[tokio::test]
    async fn fixed_slices_flush_after_each_segment() {
        let (w, mut r) = duplex(64);
        // BufWriter will buffer unless flushed; SlicedWriter should force flush after each slice
        let inner = BufWriter::new(w);
        let mut sw = SlicedWriter::new(inner, 4usize);

        // Write 10 bytes; should return 4 first
        let n1 = sw.write(b"abcdefghij").await.unwrap();
        assert_eq!(n1, 4);
        // After returning, flush should have been called: first 4 bytes should be visible
        let mut buf = [0u8; 4];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abcd");

        // Next write returns next 4
        let n2 = sw.write(b"efghij").await.unwrap();
        assert_eq!(n2, 4);
        let mut buf2 = [0u8; 4];
        r.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"efgh");
    }

    #[tokio::test]
    async fn vectored_respects_slice_size() {
        let (mut w, mut r) = duplex(128);
        let mut sw = SlicedWriter::new(&mut w, 5usize);
        let a = io::IoSlice::new(b"hello");
        let b = io::IoSlice::new(b"world");
        let n = sw.write_vectored(&[a, b]).await.unwrap();
        assert!(n <= 5);
        sw.flush().await.unwrap();

        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, b"hello"[..n].to_vec());
    }

    #[tokio::test]
    async fn dynamic_size_runtime_update() {
        let (mut w, mut r) = duplex(64);
        let size = DynamicSize::new(2);
        let mut sw = SlicedWriter::new(&mut w, size.clone());

        let n1 = sw.write(b"xyz").await.unwrap();
        assert_eq!(n1, 2);
        size.set(0); // disable slicing
        let n2 = sw.write(&b"xyz"[2..]).await.unwrap();
        assert!(n2 >= 1);
        sw.flush().await.unwrap();

        let mut buf = [0u8; 3];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"xyz");
    }

    use tokio::io::AsyncBufReadExt;

    /// A tiny test sink to simulate partial writes and observe flush/shutdown calls.
    #[derive(Default)]
    struct LimitedSink {
        /// Max bytes this sink will report as written per call.
        limit: usize,
        /// Captured bytes written so far.
        written: Vec<u8>,
        /// Number of times `flush` was called.
        flushed: usize,
        /// Number of times `shutdown` was called.
        shutdowns: usize,
        /// Number of times an empty write was attempted.
        empty_write_calls: usize,
        /// Whether this sink reports vectored-write support.
        advertise_vectored: bool,
    }

    impl LimitedSink {
        fn with_limit(limit: usize) -> Self {
            Self {
                limit,
                ..Self::default()
            }
        }
        fn with_limit_vectored(limit: usize, advertise_vectored: bool) -> Self {
            Self {
                limit,
                advertise_vectored,
                ..Self::default()
            }
        }
    }

    impl AsyncWrite for LimitedSink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if buf.is_empty() {
                self.empty_write_calls += 1;
                return Poll::Ready(Ok(0));
            }
            let n = buf.len().min(self.limit);
            self.written.extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let mut remaining = self.limit;
            let mut n = 0;
            for s in bufs {
                if remaining == 0 {
                    break;
                }
                let take = s.len().min(remaining);
                self.written.extend_from_slice(&s[..take]);
                remaining -= take;
                n += take;
            }
            Poll::Ready(Ok(n))
        }

        fn is_write_vectored(&self) -> bool {
            self.advertise_vectored
        }

        fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushed += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.shutdowns += 1;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn vectored_cross_boundary_flushes() {
        let (w, mut r) = duplex(64);
        // BufWriter ensures we can observe the flush boundary.
        let inner = BufWriter::new(w);
        let mut sw = SlicedWriter::new(inner, 7usize);

        let a = io::IoSlice::new(b"hello");
        let b = io::IoSlice::new(b"world");
        let n = sw.write_vectored(&[a, b]).await.unwrap();
        assert_eq!(n, 7, "should write up to the slice limit");

        let mut chunk = [0u8; 7];
        r.read_exact(&mut chunk).await.unwrap();
        assert_eq!(
            &chunk, b"hellowo",
            "first chunk must be flushed and visible"
        );

        // Write the rest; should flush again.
        let n2 = sw
            .write_vectored(&[io::IoSlice::new(b"rld")])
            .await
            .unwrap();
        assert_eq!(n2, 3);
        let mut rest = [0u8; 3];
        r.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"rld");
    }

    #[tokio::test]
    async fn zero_size_disables_slicing_for_vectored() {
        let (mut w, mut r) = duplex(128);
        let mut sw = SlicedWriter::new(&mut w, 0usize);

        let a = io::IoSlice::new(b"foo");
        let b = io::IoSlice::new(b"barbaz");
        let n = sw.write_vectored(&[a, b]).await.unwrap();
        sw.flush().await.unwrap();

        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"foobarbaz");
    }

    #[tokio::test]
    async fn empty_write_returns_zero_and_does_not_flush() {
        let mut sink = LimitedSink::with_limit(64);
        {
            let mut sw = SlicedWriter::new(&mut sink, 8usize);
            let n = sw.write(&[]).await.unwrap();
            assert_eq!(n, 0);
            // drop(sw) to release &mut borrow
        }
        assert_eq!(sink.empty_write_calls, 1);
        assert_eq!(
            sink.flushed, 0,
            "no flush should be issued for empty writes"
        );
        assert!(sink.written.is_empty());
    }

    #[tokio::test]
    async fn partial_inner_write_is_respected_and_flushed() {
        let mut sink = LimitedSink::with_limit(3);
        {
            let mut sw = SlicedWriter::new(&mut sink, 10usize); // slice bigger than limit
            let n = sw.write(b"abcdef").await.unwrap();
            assert_eq!(n, 3, "wrapper must return inner's reported write size");
            // Drop to allow inspecting `sink` fields.
        }
        assert_eq!(sink.written, b"abc");
        assert_eq!(sink.flushed, 1, "flush must be called after the write");
    }

    #[tokio::test]
    async fn delegates_is_write_vectored() {
        let mut sink = LimitedSink::with_limit_vectored(8, true);
        let sw = SlicedWriter::new(&mut sink, 4usize);
        assert!(
            sw.is_write_vectored(),
            "should reflect inner writer capability"
        );
    }

    #[tokio::test]
    async fn many_iovecs_size_one_only_first_byte_written() {
        let mut sink = LimitedSink::with_limit_vectored(1, true);
        {
            let mut sw = SlicedWriter::new(&mut sink, 1usize);
            // Even with many iovecs, the wrapper should only pass 1 byte total.
            let bufs: Vec<io::IoSlice<'_>> =
                vec![vec![io::IoSlice::new(b"ABC"), io::IoSlice::new(b"DEF")]; 50]
                    .into_iter()
                    .flatten()
                    .collect();
            let n = sw.write_vectored(&bufs).await.unwrap();
            assert_eq!(n, 1);
        }
        assert_eq!(sink.written, b"A");
        assert_eq!(sink.flushed, 1, "flush after the limited vectored write");
    }

    #[tokio::test]
    async fn vectored_partial_inner_write_flushes() {
        let mut sink = LimitedSink::with_limit_vectored(3, true);
        {
            let mut sw = SlicedWriter::new(&mut sink, 8usize);
            let a = io::IoSlice::new(b"hello");
            let b = io::IoSlice::new(b"world");
            let n = sw.write_vectored(&[a, b]).await.unwrap();
            assert_eq!(n, 3, "should surface the inner writer's partial result");
        }
        assert_eq!(sink.written, b"hel");
        assert_eq!(sink.flushed, 1, "flush must follow the partial inner write");
    }

    #[tokio::test]
    async fn async_bufread_passthrough_works() {
        let (mut w, r) = duplex(64);
        let br = tokio::io::BufReader::new(r);
        // SlicedWriter should transparently proxy AsyncBufRead.
        let mut sw = SlicedWriter::new(br, 4usize);

        w.write_all(b"hello\nworld").await.unwrap();
        w.shutdown().await.unwrap();

        let mut line = String::new();
        let n = sw.read_line(&mut line).await.unwrap();
        assert_eq!(n, 6);
        assert_eq!(line, "hello\n");
    }

    #[tokio::test]
    async fn shutdown_delegates() {
        let (mut w, mut r) = duplex(16);
        let mut sw = SlicedWriter::new(&mut w, 4usize);

        sw.write_all(b"hi").await.unwrap();
        sw.shutdown().await.unwrap();

        let mut buf = [0u8; 2];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[tokio::test]
    async fn dynamic_size_after_first_write_vectored_and_scalar() {
        let (w, mut r) = duplex(64);
        let size = DynamicSize::new(2);
        let inner = BufWriter::new(w);
        let mut sw = SlicedWriter::new(inner, size.clone());

        // First: limited to 2
        let n1 = sw.write(b"abcdef").await.unwrap();
        assert_eq!(n1, 2);
        let mut first = [0u8; 2];
        r.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"ab");

        // Increase size at runtime, ensure new limit applies next call
        size.set(3);
        let a = io::IoSlice::new(b"cde");
        let b = io::IoSlice::new(b"fgh");
        let n2 = sw.write_vectored(&[a, b]).await.unwrap();
        assert_eq!(n2, 3);

        let mut next = [0u8; 3];
        r.read_exact(&mut next).await.unwrap();
        assert_eq!(&next, b"cde");
    }
}
