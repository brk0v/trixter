//! Random corruption adapter for Tokio I/O streams.
//!
//! [`Corrupter`] wraps any `AsyncRead`/`AsyncWrite` (or `AsyncBufRead`) and, according to a
//! [`Probability`] source, prepends the data flowing through it with freshly generated random
//! bytes. Reads are satisfied entirely with random data when corruption triggers (read from the inner doesn't happen),
//! while writes enqueue a random payload to be forwarded before the caller's real buffer.
//!
//! ## Use cases
//! - fuzzing higher layers by injecting garbage into I/O streams
//! - simulating frame corruption during integration tests
//! - exercising retry/error handling paths when paired with other [`tokio-netem`] adapters
//!
//! ## Static configuration
//! ```no_run
//! use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
//! use tokio::net::TcpStream;
//! use tokio_netem::corrupter::Corrupter;
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let stream = TcpStream::connect("127.0.0.1:12345").await?;
//! let mut stream = Corrupter::new(stream, 0.10f64);
//!
//! stream.write_all(b"ping").await?; // may send random bytes when corruption triggers
//! let mut buf = vec![0; 4];
//! let _ = stream.read(&mut buf).await?; // may return random bytes when corruption triggers
//! # Ok(()) }
//! ```
//!
//! ## Dynamic configuration
//! ```no_run
//! use std::sync::Arc;
//! use tokio::io::{self, AsyncWriteExt};
//! use tokio::net::TcpStream;
//! use tokio_netem::corrupter::Corrupter;
//! use tokio_netem::probability::DynamicProbability;
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let prob: Arc<DynamicProbability> = DynamicProbability::new(0.0)?; // start disabled
//! let mut stream = Corrupter::new(TcpStream::connect("127.0.0.1:12345").await?, prob.clone());
//!
//! prob.set(0.50)?;           // enable 50% corruption
//! stream.write_all(b"packet").await?;
//! prob.set(0.0)?;            // disable without rebuilding the pipeline
//! stream.write_all(b"stable").await?;
//! # Ok(()) }
//! ```
//!
//! ## Under the hood
//! - Corruption decisions compare a fresh `u64` draw against [`Probability::threshold`].
//! - Random payloads come from an internal [`SmallRng`] seeded via `thread_rng` or the provided seed.
//! - Writes queue the synthetic buffer until it has been flushed through the inner writer.
//!
//! For determinism in tests, construct the adapter with [`Corrupter::from_seed`] or
//! [`Corrupter::from_rng`].
use std::{
    fmt, io,
    pin::Pin,
    task::{ready, Context, Poll},
};

use bytes::{Buf, Bytes, BytesMut};
use pin_project::pin_project;
use rand::{rng, rngs::SmallRng, Rng, RngCore, SeedableRng};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};

use crate::{io::ResetLinger, probability::Probability};

const MIN_RANDOM_CORRUPT_BUFFER_SIZE: usize = 1;
const MAX_RANDOM_CORRUPT_BUFFER_SIZE: usize = 16 * 1024; // Full TLS record

#[inline]
fn should_corrupt<P: Probability>(rng: &mut SmallRng, prob: &P) -> bool {
    let threshold = prob.threshold();
    threshold != 0 && rng.next_u64() < threshold
}

/// Randomly replaces I/O traffic with synthetic bytes using a configurable probability.
#[pin_project]
pub struct Corrupter<T, P> {
    #[pin]
    inner: T,
    prob: P,
    rng: SmallRng,
    write_buffer: Option<Bytes>,
}

impl<T, P: Probability> Corrupter<T, P> {
    /// Creates a `Corrupter` seeded from thread-local entropy.
    ///
    /// Use this for production-style randomness where reproducibility is not required.
    pub fn new(inner: T, prob: P) -> Self {
        Corrupter {
            inner,
            prob,
            rng: SmallRng::from_rng(&mut rng()),
            write_buffer: None,
        }
    }

    /// Creates a `Corrupter` with deterministic output given `seed`.
    ///
    /// Handy for unit tests that need stable corruption patterns.
    pub fn from_seed(inner: T, prob: P, seed: [u8; 32]) -> Self {
        Corrupter {
            inner,
            prob,
            rng: SmallRng::from_seed(seed),
            write_buffer: None,
        }
    }

    /// Creates a `Corrupter` seeded from an arbitrary RNG.
    ///
    /// The provided RNG is only used to initialise the internal `SmallRng`; it is not retained.
    pub fn from_rng(inner: T, prob: P, rng: &mut impl RngCore) -> Self {
        Corrupter {
            inner,
            prob,
            rng: SmallRng::from_rng(rng),
            write_buffer: None,
        }
    }

    #[inline]
    fn random_data_size(rng: &mut SmallRng, len: usize) -> Bytes {
        let mut buf = BytesMut::with_capacity(len);
        // SAFETY: set_len without initialization, then fill with random bytes
        unsafe {
            buf.set_len(len);
        }
        rng.fill_bytes(buf.as_mut());
        buf.freeze()
    }

    #[inline]
    fn random_data(rng: &mut SmallRng) -> Bytes {
        let size =
            rng.random_range(MIN_RANDOM_CORRUPT_BUFFER_SIZE..=MAX_RANDOM_CORRUPT_BUFFER_SIZE);

        Self::random_data_size(rng, size)
    }
}

impl<T: ResetLinger, P> ResetLinger for Corrupter<T, P> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead + Unpin, P: Probability> AsyncBufRead for Corrupter<T, P> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<R: AsyncRead + Unpin, P: Probability> AsyncRead for Corrupter<R, P> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().project();

        if should_corrupt(this.rng, &*this.prob) {
            let len = buf.remaining();
            if len > 0 {
                let data = Self::random_data_size(this.rng, len);
                buf.put_slice(&data);
            }
            return Poll::Ready(Ok(()));
        }

        this.inner.poll_read(cx, buf)
    }
}

impl<W: AsyncWrite + Unpin, P: Probability> AsyncWrite for Corrupter<W, P> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.project();

        loop {
            if this.write_buffer.is_some() {
                let write_buffer = this.write_buffer.as_mut().expect("buffer must be set");

                while !write_buffer.is_empty() {
                    let size = ready!(this.inner.as_mut().poll_write(cx, write_buffer))?;
                    if size == 0 {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write zero bytes",
                        )));
                    }
                    write_buffer.advance(size);
                }

                this.write_buffer.take(); // clear buffer
                break; // write a real buffer now
            }

            if should_corrupt(this.rng, &*this.prob) {
                let data = Self::random_data(this.rng);
                this.write_buffer.replace(data);
            } else {
                break; // not triggered write a real buffer
            }
        }

        this.inner.poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().project();
        this.inner.poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().project();
        this.inner.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        // TODO: implement
        let this = self.as_mut().project();
        this.inner.poll_write_vectored(cx, bufs)
    }
}

impl<RW: fmt::Debug, P> fmt::Debug for Corrupter<RW, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::IoSlice;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    use tokio::io::{self, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    const SEED: [u8; 32] = [7; 32];

    #[derive(Clone, Copy, Default)]
    struct ConstProb {
        prob: f64,
        threshold: u64,
    }

    impl ConstProb {
        fn never() -> Self {
            Self {
                prob: 0.0,
                threshold: 0,
            }
        }

        fn always() -> Self {
            Self {
                prob: 1.0,
                threshold: u64::MAX,
            }
        }
    }

    impl Probability for ConstProb {
        fn probability(&self) -> f64 {
            self.prob
        }

        fn threshold(&self) -> u64 {
            self.threshold
        }
    }

    #[derive(Clone, Default)]
    struct RecordingWriter {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl RecordingWriter {
        fn new() -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
            let store = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    writes: store.clone(),
                },
                store,
            )
        }
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.writes.lock().unwrap().push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let total: usize = bufs.iter().map(|b| b.len()).sum();
            let mut data = Vec::with_capacity(total);
            for slice in bufs {
                data.extend_from_slice(slice);
            }
            this.writes.lock().unwrap().push(data);
            Poll::Ready(Ok(total))
        }
    }

    #[derive(Clone)]
    struct LimitedRecordingWriter {
        writes: Arc<Mutex<Vec<u8>>>,
        limit: usize,
    }

    impl LimitedRecordingWriter {
        fn new(limit: usize) -> (Self, Arc<Mutex<Vec<u8>>>) {
            let store = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    writes: store.clone(),
                    limit,
                },
                store,
            )
        }
    }

    impl AsyncWrite for LimitedRecordingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let n = buf.len().min(this.limit);
            if n == 0 {
                return Poll::Ready(Ok(0));
            }
            this.writes.lock().unwrap().extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct SequenceProb {
        remaining: Mutex<VecDeque<bool>>,
        last: AtomicBool,
    }

    impl SequenceProb {
        fn new(pattern: Vec<bool>) -> Self {
            Self {
                remaining: Mutex::new(VecDeque::from(pattern)),
                last: AtomicBool::new(false),
            }
        }
    }

    impl Probability for SequenceProb {
        fn probability(&self) -> f64 {
            if self.last.load(Ordering::SeqCst) {
                1.0
            } else {
                0.0
            }
        }

        fn threshold(&self) -> u64 {
            let flag = self.remaining.lock().unwrap().pop_front().unwrap_or(false);
            self.last.store(flag, Ordering::SeqCst);
            if flag {
                u64::MAX
            } else {
                0
            }
        }
    }

    #[tokio::test]
    async fn read_passes_through_when_probability_zero() {
        let (mut w, r) = tokio::io::duplex(32);
        let mut reader = Corrupter::new(r, ConstProb::never());

        w.write_all(b"hello").await.unwrap();
        w.shutdown().await.unwrap();

        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn read_corrupts_when_probability_one() {
        let (_w, r) = tokio::io::duplex(8);
        let mut reader = Corrupter::from_seed(r, ConstProb::always(), SEED);

        let mut buf = [0u8; 6];
        reader.read_exact(&mut buf).await.unwrap();

        let mut rng = SmallRng::from_seed(SEED);
        let _ = rng.next_u64();
        let expected = Corrupter::<(), ConstProb>::random_data_size(&mut rng, buf.len());
        assert_eq!(&buf, expected.as_ref());
    }

    #[tokio::test]
    async fn write_passes_through_when_probability_zero() {
        let (inner, store) = RecordingWriter::new();
        let mut writer = Corrupter::new(inner, ConstProb::never());

        writer.write_all(b"payload").await.unwrap();

        let log = store.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], b"payload");
    }

    #[tokio::test]
    async fn write_injects_random_chunk_before_payload() {
        let (inner, store) = RecordingWriter::new();
        let mut writer = Corrupter::from_seed(inner, ConstProb::always(), SEED);

        writer.write_all(b"data").await.unwrap();

        let log = store.lock().unwrap();
        assert_eq!(log.len(), 2, "expected injected chunk followed by payload");
        assert!(!log[0].is_empty(), "injected chunk must contain data");
        assert_eq!(log[1], b"data");
    }

    #[tokio::test]
    async fn write_injected_bytes_drain_before_payload_with_partial_inner_writes() {
        let (inner, store) = LimitedRecordingWriter::new(2);
        let prob = SequenceProb::new(vec![true, false]);
        let mut writer = Corrupter::from_seed(inner, prob, SEED);

        writer.write_all(b"PAYLOAD").await.unwrap();
        writer.flush().await.unwrap();

        let captured = store.lock().unwrap().clone();

        let mut expected_rng = SmallRng::from_seed(SEED);
        let _ = expected_rng.next_u64(); // consumed by should_corrupt
        let injected = Corrupter::<(), SequenceProb>::random_data(&mut expected_rng);
        let injected = injected.to_vec();

        assert!(
            captured.starts_with(&injected),
            "random chunk must be forwarded before payload"
        );
        assert_eq!(&captured[injected.len()..], b"PAYLOAD");
    }
}
