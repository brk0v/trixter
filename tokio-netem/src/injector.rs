//! Injection adapters for Tokio I/O streams.
//!
//! The [`ReadInjector`] and [`WriteInjector`] wrappers splice arbitrary byte buffers into a live
//! pipeline by draining a `tokio::sync::mpsc::Receiver<Bytes>` before polling the wrapped I/O.
//! Use them to stage protocol glitches, late control frames, or replay attacks without touching
//! the core business logic.
//!
//! ## Read path
//! - Injected buffers are surfaced as additional reads before the underlying reader is polled.
//! - When the channel is empty (or closed), reads fall back to the wrapped `AsyncRead`.
//!
//! ## Write path
//! - Injected buffers are flushed to the inner writer before the caller's pending write.
//! - Backpressure is respected: queued payloads finish writing before normal I/O resumes.
//!
//! ## Example
//! ```no_run
//! use bytes::Bytes;
//! use tokio::io::{self, AsyncReadExt, AsyncWriteExt, duplex};
//! use tokio::sync::mpsc;
//! use tokio_netem::injector::{ReadInjector, WriteInjector};
//!
//! # #[tokio::main]
//! # async fn main() -> io::Result<()> {
//! let (reader_half, writer_half) = duplex(64);
//! let (tx_read, rx_read) = mpsc::channel(4);
//! let (tx_write, rx_write) = mpsc::channel(4);
//! let mut reader = ReadInjector::new(reader_half, rx_read);
//! let mut writer = WriteInjector::new(writer_half, rx_write);
//!
//! tx_read.send(Bytes::from_static(b"hello-")).await.unwrap();
//! tx_write.send(Bytes::from_static(b"inject-")).await.unwrap();
//!
//! writer.write_all(b"payload").await?;
//! writer.flush().await?;
//! let mut buf = vec![0; 20];
//! reader.read_exact(&mut buf).await?;
//! assert_eq!(&buf, b"hello-inject-payload");
//! # Ok(()) }
//! ```
use std::{
    fmt, io,
    pin::Pin,
    task::{ready, Context, Poll},
};

use bytes::{Buf, Bytes};
use pin_project::pin_project;
use tokio::{
    io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf},
    sync::mpsc,
};

use crate::io::ResetLinger;

/// Injects arbitrary [`Bytes`] into a reader by draining an `mpsc::Receiver` before polling the
/// inner [`AsyncRead`].
///
/// Each injected buffer is consumed and replayed as synthetic data. When the channel is empty (or
/// closed), reads fall back to the wrapped reader.
#[pin_project]
pub struct ReadInjector<T> {
    #[pin]
    inner: T,

    buffer: Option<Bytes>,
    inject_rx: mpsc::Receiver<Bytes>,
}

impl<T> ReadInjector<T> {
    /// Wraps `inner` and delivers queued payloads from `inject_rx` as extra reads.
    ///
    /// Callers retain the sending end of the channel and push payloads whenever they want the next
    /// read on `ReadInjector` to observe synthetic bytes.
    pub fn new(inner: T, inject_rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            inner,
            inject_rx,
            buffer: None,
        }
    }
}

impl<T: ResetLinger> ResetLinger for ReadInjector<T> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead + Unpin> AsyncBufRead for ReadInjector<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ReadInjector<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();

        loop {
            if this.buffer.is_some() {
                let buffer = this.buffer.as_mut().expect("buffer must be set");

                let n = buf.remaining().min(buffer.len());
                let chunk = buffer.split_to(n);
                buf.put_slice(&chunk);

                if buffer.is_empty() {
                    this.buffer.take();
                }

                return Poll::Ready(Ok(()));
            }

            match this.inject_rx.poll_recv(cx) {
                Poll::Ready(Some(buffer)) => {
                    this.buffer.replace(buffer);
                    continue;
                }
                Poll::Ready(None) | Poll::Pending => break, // ignore closed or pending channel
            };
        }

        this.inner.poll_read(cx, buf)
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ReadInjector<W> {
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

impl<RW: fmt::Debug> fmt::Debug for ReadInjector<RW> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// Injects [`Bytes`] into the write path of an [`AsyncWrite`] by draining an `mpsc::Receiver`
/// before forwarding caller-provided buffers.
///
/// Injected buffers are written in full (with backpressure respected) prior to the actual write
/// request that triggered polling.
#[pin_project]
pub struct WriteInjector<T> {
    #[pin]
    inner: T,

    buffer: Option<Bytes>,
    inject_rx: mpsc::Receiver<Bytes>,
}

impl<T> WriteInjector<T> {
    /// Wraps `inner` and writes queued payloads from `inject_rx` before caller-provided buffers.
    ///
    /// Ideal for crafting synthetic frames or fault injections without altering the main writer.
    pub fn new(inner: T, inject_rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            inner,
            inject_rx,
            buffer: None,
        }
    }
}

impl<T: ResetLinger> ResetLinger for WriteInjector<T> {
    fn set_reset_linger(&mut self) -> io::Result<()> {
        self.inner.set_reset_linger()
    }
}

impl<T: AsyncBufRead + Unpin> AsyncBufRead for WriteInjector<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.project().inner.poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.project().inner.consume(amt)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for WriteInjector<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for WriteInjector<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.project();

        loop {
            if this.buffer.is_some() {
                let buffer = this.buffer.as_mut().expect("buffer must be set");

                while !buffer.is_empty() {
                    let size = ready!(this.inner.as_mut().poll_write(cx, buffer))?;
                    if size == 0 {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write zero bytes",
                        )));
                    }
                    buffer.advance(size);
                }

                this.buffer.take(); // clear buffer
            }

            match this.inject_rx.poll_recv(cx) {
                Poll::Ready(Some(buffer)) => {
                    this.buffer.replace(buffer);
                    continue;
                }
                Poll::Ready(None) | Poll::Pending => break, // ignore closed or not ready channel
            };
        }

        this.inner.as_mut().poll_write(cx, buf)
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
        // TODO: implement
        self.project().inner.poll_write_vectored(cx, bufs)
    }
}

impl<RW: fmt::Debug> fmt::Debug for WriteInjector<RW> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::IoSlice;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use tokio::io::{self, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let mut remaining = this.limit;
            if remaining == 0 {
                return Poll::Ready(Ok(0));
            }

            let mut written = 0;
            for slice in bufs {
                if remaining == 0 {
                    break;
                }
                let take = slice.len().min(remaining);
                if take == 0 {
                    continue;
                }
                this.writes
                    .lock()
                    .unwrap()
                    .extend_from_slice(&slice[..take]);
                written += take;
                remaining -= take;
                if take < slice.len() {
                    break;
                }
            }

            Poll::Ready(Ok(written))
        }
    }

    #[tokio::test]
    async fn read_injector_drains_injected_bytes_first() {
        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Bytes::from_static(b"boom")).unwrap();
        drop(tx);

        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"data").await.unwrap();
        writer.shutdown().await.unwrap();

        let mut reader = ReadInjector::new(reader, rx);

        let mut injected = [0u8; 4];
        reader.read_exact(&mut injected).await.unwrap();
        assert_eq!(&injected, b"boom");

        let mut payload = [0u8; 4];
        reader.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"data");
    }

    #[tokio::test]
    async fn write_injector_writes_pending_chunks_before_payload() {
        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Bytes::from_static(b"one")).unwrap();
        tx.try_send(Bytes::from_static(b"two")).unwrap();
        drop(tx);

        let (inner, store) = RecordingWriter::new();
        let mut writer = WriteInjector::new(inner, rx);

        writer.write_all(b"payload").await.unwrap();

        let log = store.lock().unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0], b"one");
        assert_eq!(log[1], b"two");
        assert_eq!(log[2], b"payload");
    }

    #[tokio::test]
    async fn write_injector_pass_through_when_no_injections() {
        let (_tx, rx) = mpsc::channel(4);
        let (inner, store) = RecordingWriter::new();
        let mut writer = WriteInjector::new(inner, rx);

        writer.write_all(b"payload").await.unwrap();

        let log = store.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], b"payload");
    }

    #[tokio::test]
    async fn read_injector_handles_partial_reads_from_buffer() {
        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Bytes::from_static(b"abcdef")).unwrap();
        drop(tx);

        let (mut writer, reader) = tokio::io::duplex(32);
        tokio::spawn(async move {
            writer.write_all(b"payload").await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let mut reader = ReadInjector::new(reader, rx);

        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"abc");

        let mut buf2 = [0u8; 3];
        reader.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"def");

        let mut buf3 = [0u8; 7];
        reader.read_exact(&mut buf3).await.unwrap();
        assert_eq!(&buf3, b"payload");
    }

    #[tokio::test]
    async fn write_injector_preserves_injected_bytes_across_partial_inner_writes() {
        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Bytes::from_static(b"injected")).unwrap();
        drop(tx);

        let (inner, store) = LimitedRecordingWriter::new(3);
        let mut writer = WriteInjector::new(inner, rx);

        writer.write_all(b"PAYLOAD").await.unwrap();
        writer.flush().await.unwrap();

        let log = store.lock().unwrap();
        assert_eq!(log.len(), b"injectedPAYLOAD".len());
        assert_eq!(&log[..b"injected".len()], b"injected");
        assert_eq!(&log[b"injected".len()..], b"PAYLOAD");
    }
}
