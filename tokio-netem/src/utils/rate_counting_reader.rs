//! Throughput utilities for asynchronous readers.
//!
//! [`RateCountingReader`] wraps any `AsyncRead` and accumulates bytes plus a start timestamp so you
//! can query a simple average bytes-per-second figure—handy for asserting throttling behaviour in
//! tests.
//!
//! ## Example
//! ```no_run
//! use tokio::io::{self, AsyncReadExt, AsyncWriteExt, duplex};
//! use tokio_netem::utils::rate_counting_reader::RateCountingReader;
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let (mut writer, reader) = duplex(64);
//! let mut reader = RateCountingReader::new(reader);
//!
//! tokio::spawn(async move {
//!     let _ = writer.write_all(b"hello world").await;
//! });
//!
//! let mut buf = vec![0u8; 11];
//! reader.read_exact(&mut buf).await?;
//! assert_eq!(reader.total(), 11);
//! assert!(reader.rate_bps().unwrap() > 0.0);
//! # Ok(()) }
//! ```
//!
//! ## When to use
//! - Instrument in-memory streams while tuning throttling/delay adapters.
//! - Gather coarse-grained telemetry for integration tests without pulling in heavy metrics crates.
use pin_project::pin_project;
use std::{
    fmt,
    io::IoSlice,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Counts bytes flowing through an `AsyncRead` and reports a simple average rate.
///
/// - The timer starts only after the **first non-zero** read.
/// - `rate_bps()` returns `None` until at least one byte has been read.
/// - `reset()` clears counters and the start time.
/// - Accessors expose the wrapped I/O for inspection or reuse.
#[pin_project]
pub struct RateCountingReader<T> {
    #[pin]
    inner: T,
    total_bytes: u64,
    start: Option<Instant>,
}

impl<T> RateCountingReader<T> {
    /// Wraps `inner`, recording the volume and timing of subsequent reads.
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            total_bytes: 0,
            start: None,
        }
    }

    /// Total bytes read so far (saturating).
    #[inline]
    pub fn total(&self) -> u64 {
        self.total_bytes
    }

    /// Start `Instant` (when the first non-zero read happened).
    #[inline]
    pub fn start_instant(&self) -> Option<Instant> {
        self.start
    }

    /// Average read rate since `start_instant()` in bytes per second.
    /// Returns `None` until at least one byte has been read.
    pub fn rate_bps(&self) -> Option<f64> {
        let start = self.start?;
        // Clamp denominator to avoid FP blow-ups on extremely small intervals.
        let elapsed = start.elapsed().as_secs_f64().max(1e-6);
        Some(self.total_bytes as f64 / elapsed)
    }

    /// Reset counters and timer.
    #[inline]
    pub fn reset(&mut self) {
        self.total_bytes = 0;
        self.start = None;
    }

    /// Immutable access to the inner I/O.
    #[inline]
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Mutable access to the inner I/O.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Consume the wrapper and return the inner I/O.
    #[inline]
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: fmt::Debug> fmt::Debug for RateCountingReader<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateCountingReader")
            .field("inner", &self.inner)
            .field("total_bytes", &self.total_bytes)
            .field("start", &self.start)
            .finish()
    }
}

impl<T: AsyncRead> AsyncRead for RateCountingReader<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut this = self.project();
        let before = buf.filled().len();

        match this.inner.as_mut().poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let after = buf.filled().len();
                let diff = after.saturating_sub(before) as u64;

                if diff > 0 {
                    *this.total_bytes = (*this.total_bytes).saturating_add(diff);
                    if this.start.is_none() {
                        *this.start = Some(Instant::now());
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<T: AsyncWrite> AsyncWrite for RateCountingReader<T> {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.project().inner.poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        self.project().inner.poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.project().inner.poll_shutdown(cx)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn counts_bytes_and_sets_start_on_first_nonzero() {
        let (client, mut server) = tokio::io::duplex(64);
        let mut reader = RateCountingReader::new(client);

        // Write some data from the "server" side.
        tokio::spawn(async move {
            let _ = server.write_all(b"hello world").await;
            let _ = server.shutdown().await;
        });

        let mut buf = vec![0u8; 11];
        reader.read_exact(&mut buf).await.unwrap();

        assert_eq!(buf, b"hello world");
        assert_eq!(reader.total(), 11);
        assert!(reader.start_instant().is_some());
        assert!(reader.rate_bps().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn zero_byte_reads_do_not_start_timer() {
        // `io::empty()` yields EOF with zero bytes.
        let mut reader = RateCountingReader::new(io::empty());
        let mut buf = [0u8; 8];
        let n = reader.read(&mut buf).await.unwrap();
        assert_eq!(n, 0);
        assert_eq!(reader.total(), 0);
        assert!(reader.rate_bps().is_none());
        assert!(reader.start_instant().is_none());
    }
}
