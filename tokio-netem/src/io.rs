//! NetEm extension traits and helpers.
//!
//! These blanket impls let you bolt the crate’s throttling, delaying, slicing,
//! adapters straight onto any `AsyncRead`/`AsyncWrite`/`AsyncBufRead` type via
//! ergonomic extension methods. Every method comes in **static** and (where meaningful)
//! **dynamic** flavors that mirror the underlying modules.
//!
//! ## Static configuration
//! ```no_run
//! use std::time::Duration;
//! use tokio::io::{self, AsyncReadExt, AsyncWriteExt, BufReader};
//! use tokio::net::TcpStream;
//! use tokio_netem::io::{NetEmReadExt, NetEmWriteExt};
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let (reader, writer) = stream.into_split();
//!
//! // Apply static throttling + latency without managing wrapper types manually.
//! let mut reader = BufReader::new(reader).throttle_reads(32);
//! let mut writer = writer
//!     .delay_writes(Duration::from_millis(25))
//!     .slice_writes(8);
//!
//! writer.write_all(b"ping").await?;
//! writer.flush().await?; // assumes the peer echoes data back
//! let mut buf = [0u8; 4];
//! reader.read_exact(&mut buf).await?;
//! # Ok(()) }
//! ```
//!
//! ## Dynamic configuration
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use tokio::io::{self, AsyncReadExt, AsyncWriteExt, BufReader};
//! use tokio::net::TcpStream;
//! use tokio_netem::io::{NetEmReadExt, NetEmWriteExt};
//! use tokio_netem::{
//!     terminator::Terminator,
//!     delayer::DynamicDuration,
//!     probability::DynamicProbability,
//!     throttler::DynamicRate,
//! };
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let rate: Arc<DynamicRate> = DynamicRate::new(0);
//! let delay: Arc<DynamicDuration> = DynamicDuration::new(Duration::from_millis(0));
//! let probability: Arc<DynamicProbability> = DynamicProbability::new(0.0)?;
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let (reader_half, writer_half) = stream.into_split();
//!
//! let mut reader = BufReader::new(reader_half).throttle_reads_dyn(rate.clone());
//! let writer_half = Terminator::new(writer_half, probability.clone());
//! let mut writer = writer_half
//!     .delay_writes_dyn(delay.clone());
//!
//! rate.set(16);                       // tweak knobs at runtime
//! delay.set(Duration::from_millis(50));
//! probability.set(0.25)?;             // 25% chance to trip after this point
//! writer.write_all(b"pong").await?;
//! let mut buf = [0u8; 4];
//! reader.read_exact(&mut buf).await?; // assumes the remote peer responds
//! # Ok(()) }
//! ```
//!
//! ## Under the hood
//! - [`NetEmReadExt`], [`NetEmWriteExt`], and friends are blanket-implemented for all Tokio
//!   I/O traits, returning the same adapters defined in `delayer`, `throttler`, `slicer`.
//! - Static variants use plain values (`usize`, `Duration`, `f64`) to avoid heap/atomic costs on
//!   the hot path; dynamic variants accept `Arc<...>` handles for lock-free runtime updates.
use std::{io, sync::Arc, time};

use bytes::Bytes;
use tokio::{
    io::{
        AsyncBufRead, AsyncRead, AsyncWrite, BufReader, BufStream, BufWriter, DuplexStream,
        SimplexStream,
    },
    sync::mpsc,
};

use crate::{
    delayer::{self, DynamicDuration},
    injector,
    slicer::{self, DynamicSize},
    throttler::{self, DynamicRate},
};

/// Request abortive close (RST-on-close) semantics where supported.
///
/// On TCP, setting `SO_LINGER{on=1, linger=0}` causes the kernel to send a TCP **RST** when the
/// socket is closed, immediately aborting the connection rather than performing a graceful
/// FIN/ACK shutdown. This is handy in tests to simulate peers that drop connections abruptly.
///
/// Types that do not support linger or where the concept is meaningless implement this as
/// a **no-op** and return `Ok(())`.
///
/// # Type behavior
/// - **`tokio::net::TcpStream`**: sets `SO_LINGER` to zero timeout (abortive close).
/// - **`UnixStream`, `UdpSocket`, buffered wrappers, in-memory streams**: no-op.
///
/// # Errors
/// Returns `io::Error` only for types where setting linger can actually fail (e.g., TCP).
///
/// # Idempotency
/// Safe to call multiple times.
pub trait ResetLinger {
    fn set_reset_linger(&mut self) -> io::Result<()>;
}

impl ResetLinger for tokio::net::TcpStream {
    /// Requests abortive-close (RST) on close where applicable.
    fn set_reset_linger(&mut self) -> io::Result<()> {
        tokio::net::TcpStream::set_linger(self, Some(time::Duration::from_secs(0)))
    }
}

impl ResetLinger for tokio::net::UnixStream {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ResetLinger for tokio::net::unix::pipe::Receiver {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ResetLinger for tokio::net::UdpSocket {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<RW> ResetLinger for BufStream<RW> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<R: AsyncRead> ResetLinger for BufReader<R> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: AsyncWrite> ResetLinger for BufWriter<W> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ResetLinger for DuplexStream {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ResetLinger for SimplexStream {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Read-side ergonomics for netem adapters.
///
/// Implemented for all `AsyncBufRead`. Methods return wrapped readers/writers to
/// simulate bandwidth limits, latency, slicing, random termination, and abrupt shutdowns.
///
/// ### Static vs dynamic knobs
/// - `*_dynamic` variants accept an `Arc<...>` configuration that can be updated at runtime.
/// - Non-dynamic variants are allocation-free and avoid atomic loads in hot paths.
///
/// # Examples
/// ```
/// use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, BufReader};
/// use tokio_netem::io::NetEmReadExt;
///
/// # async fn example() {
/// let (mut w, r) = duplex(64);
/// let mut r = BufReader::new(r).throttle_reads(32);
/// w.write_all(b"hello").await.unwrap();
/// w.flush().await.unwrap();
/// let mut buf = vec![0; 5];
/// r.read_exact(&mut buf).await.unwrap();
/// assert_eq!(&buf, b"hello");
/// # }
/// ```
pub trait NetEmReadExt: AsyncRead {
    /// Wrap this reader with a **static** throughput limit (bytes/second).
    ///
    /// Set `rate = 0` to disable throttling (pass-through).
    #[must_use]
    fn throttle_reads(self, rate: usize) -> throttler::ThrottledReader<Self, usize>
    where
        Self: AsyncBufRead + Sized,
    {
        throttler::ThrottledReader::new(self, rate)
    }

    /// Wrap this reader with a **dynamic** throughput limit (bytes/second).
    ///
    /// Updating the shared [`DynamicRate`] changes the limit for all users immediately.
    #[must_use]
    fn throttle_reads_dyn(
        self,
        rate: Arc<DynamicRate>,
    ) -> throttler::ThrottledReader<Self, Arc<DynamicRate>>
    where
        Self: AsyncBufRead + Sized,
    {
        throttler::ThrottledReader::new(self, rate)
    }

    /// Add a fixed **one-way** delay before delivering the first byte of each
    /// read **burst** from a buffered source.
    ///
    /// While the internal buffer still has data, subsequent reads aren’t delayed again.
    #[must_use]
    fn delay_reads(self, delay: time::Duration) -> delayer::DelayedReader<Self, time::Duration>
    where
        Self: AsyncBufRead + Sized,
    {
        delayer::DelayedReader::new(self, delay)
    }

    /// Same as [`delay_reads`](Self::delay_reads) but with a **dynamic** delay knob.
    ///
    /// Update the shared [`DynamicDuration`] at runtime to change latency.
    #[must_use]
    fn delay_reads_dyn(
        self,
        delay: Arc<DynamicDuration>,
    ) -> delayer::DelayedReader<Self, Arc<DynamicDuration>>
    where
        Self: AsyncBufRead + Sized,
    {
        delayer::DelayedReader::new(self, delay)
    }

    #[must_use]
    fn inject_read(self, inject_rx: mpsc::Receiver<Bytes>) -> injector::ReadInjector<Self>
    where
        Self: Sized,
    {
        injector::ReadInjector::new(self, inject_rx)
    }
}

impl<T: AsyncRead> NetEmReadExt for T {}

/// Write-side ergonomics for netem adapters.
///
/// Implemented for all `AsyncWrite`. Methods return wrapped writers to simulate
/// bandwidth limits, latency injection, slicing, probabilistic termination,
/// and abrupt shutdowns.
///
/// See also the read-side counterparts in [`NetEmReadExt`].
pub trait NetEmWriteExt: AsyncWrite {
    /// Wrap this writer with a **static** throughput limit (bytes/second).
    ///
    /// Set `rate = 0` to disable throttling (pass-through).
    #[must_use]
    fn throttle_writes(self, rate: usize) -> throttler::ThrottledWriter<Self, usize>
    where
        Self: Sized,
    {
        throttler::ThrottledWriter::new(self, rate)
    }

    /// Wrap this writer with a **dynamic** throughput limit (bytes/second).
    ///
    /// Updating the shared [`DynamicRate`] changes the limit for all users immediately.
    #[must_use]
    fn throttle_writes_dyn(
        self,
        rate: Arc<DynamicRate>,
    ) -> throttler::ThrottledWriter<Self, Arc<DynamicRate>>
    where
        Self: Sized,
    {
        throttler::ThrottledWriter::new(self, rate)
    }

    /// Add a fixed delay **before each write and shutdown** operation.
    #[must_use]
    fn delay_writes(self, delay: time::Duration) -> delayer::DelayedWriter<Self, time::Duration>
    where
        Self: Sized,
    {
        delayer::DelayedWriter::new(self, delay)
    }

    /// Same as [`delay_writes`](Self::delay_writes) but with a **dynamic** delay knob.
    ///
    /// Update the shared [`DynamicDuration`] at runtime to change latency.
    #[must_use]
    fn delay_writes_dyn(
        self,
        delay: Arc<delayer::DynamicDuration>,
    ) -> delayer::DelayedWriter<Self, Arc<DynamicDuration>>
    where
        Self: Sized,
    {
        delayer::DelayedWriter::new(self, delay)
    }

    /// Force writes to be **sliced** into fixed-size segments, flushing after each.
    ///
    /// A size of `0` means pass-through (no slicing).
    #[must_use]
    fn slice_writes(self, size: usize) -> slicer::SlicedWriter<Self, usize>
    where
        Self: Sized,
    {
        slicer::SlicedWriter::new(self, size)
    }

    /// Dynamic version of [`slice_writes`](Self::slice_writes).
    #[must_use]
    fn slice_writes_dyn(
        self,
        size: Arc<DynamicSize>,
    ) -> slicer::SlicedWriter<Self, Arc<DynamicSize>>
    where
        Self: Sized,
    {
        slicer::SlicedWriter::new(self, size)
    }

    #[must_use]
    fn inject_write(self, inject_rx: mpsc::Receiver<Bytes>) -> injector::WriteInjector<Self>
    where
        Self: Sized,
    {
        injector::WriteInjector::new(self, inject_rx)
    }
}

impl<T: AsyncWrite> NetEmWriteExt for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
    use tokio::time;

    #[tokio::test(start_paused = true)]
    async fn ext_throttled_write_and_slice_and_delay() {
        let (w, mut r) = duplex(256);
        let rate = 16usize;
        let size = 4usize;
        let delay = time::Duration::from_millis(10);

        let mut w = w
            .throttle_writes(rate)
            .slice_writes(size)
            .delay_writes(delay);

        w.write_all(b"hello world").await.unwrap();
        w.flush().await.unwrap();

        let mut buf = vec![0u8; 11];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello world");
    }
}
