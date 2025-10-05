# tokio-netem

`tokio-netem` provides a toolbox of Tokio `AsyncRead`/`AsyncWrite` adapters that let you emulate
latency, throttling, slicing, terminations, forced shutdowns, data injections and data corruption without touching your application
code. Compose them around `TcpStream` (or any Tokio I/O type) to run realistic integration tests and
chaos experiments.

## Table of Contents
- [Why tokio-netem?](#why-tokio-netem)
- [Installation](#installation)
- [Adapters](#adapters)
  - [DelayedReader & DelayedWriter](#delayedreader--delayedwriter)
  - [SlicedWriter](#slicedwriter)
  - [ThrottledReader & ThrottledWriter](#throttledreader--throttledwriter)
  - [Corrupter](#corrupter)
  - [ReadInjector & WriteInjector](#readinjector--writeinjector)
  - [Terminator](#terminator)
  - [Shutdowner](#shutdowner)

## Why tokio-netem?
- Reproduce hard-to-trigger network edge cases in unit and integration tests.
- Model network delays, fragmented writes, corruption, or partial packet delivery without external proxies.
- Flip failures on and off at runtime to probe resilience and backoff strategies.
- Share dynamic knobs across tasks so admin endpoints or test harnesses can adjust conditions
  without rebuilding pipelines.

## Installation
Add the crate to your `Cargo.toml`:

```toml
[dependencies]
tokio-netem = "0.1"
```

## Adapters
Each adapter accepts either a **static** configuration (plain `Duration`, `usize`, or `f64`) or a
**dynamic** handle (`Arc<...>`). Static setups are ideal for fixed scenarios in documentation or
smoke tests. Dynamic handles shine when you want to tweak behavior while the pipeline is running.
All examples below use a TCP client stream to emphasise end-to-end usage.

In practice the **writer half** is the most common place to inject chaos: outbound adapters such as
`DelayedWriter`, `SlicedWriter`, `ThrottledWriter`, and `Terminator` add near-zero overhead because
they operate on the data already in-flight and do not require extra buffering in memory the way a
reader-side shim might. Start perturbing on the write path first, then layer reader adapters if you
need full-duplex scenarios.

## NetEm extension traits
There are two extension traits – `NetEmReadExt` and `NetEmWriteExt` which significantly simplify usage of some adapters (not all supported, see the full documentation):

```rust,no_run
use std::io;
use tokio::net::TcpStream;
use tokio_netem::io::NetEmWriteExt;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut stream = TcpStream::connect("localhost:80")
        .await?
        .throttle_writes(32 * 1024) // 32 KB/s
        .slice_writes(16); // fragment writes
    Ok(())
}
```

### `DelayedReader` & `DelayedWriter`

Adds latency before reads or writes progress. `DelayedReader` delays the *first* byte of each buffered
burst; `DelayedWriter` delays every outbound write.

**Static config example**

```rust,no_run
use std::time::Duration;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_netem::io::{NetEmReadExt, NetEmWriteExt};

#[tokio::main]
async fn main() -> io::Result<()> {
    let stream = TcpStream::connect("127.0.0.1:5555").await?;

    let mut stream = stream.delay_writes(Duration::from_millis(25));
    let mut stream = BufReader::new(stream).delay_reads(Duration::from_millis(10));

    stream.write_all(b"ping").await?; // ~25 ms pause before bytes depart
    stream.flush().await?;

    let mut line = String::new();
    stream.read_line(&mut line).await?; // first buffered byte is held for ~10 ms
    Ok(())
}
```

**Dynamic config example**

```rust,no_run
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_netem::delayer::DynamicDuration;
use tokio_netem::io::NetEmWriteExt;

#[tokio::main]
async fn main() -> io::Result<()> {
    let knob: Arc<DynamicDuration> = DynamicDuration::new(Duration::ZERO);
    let stream = TcpStream::connect("127.0.0.1:5555").await?;
    let mut stream = stream.delay_writes_dyn(knob.clone());

    stream.write_all(b"fast").await?; // no delay yet
    knob.set(Duration::from_millis(80));
    stream.write_all(b"slow").await?; // takes ~80 ms now
    Ok(())
}
```

### `SlicedWriter`
Slices outbound writes into fixed-size chunks and flushes between them, emulating MTU-like behavior.

**Static config example**

```rust,no_run
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_netem::io::NetEmWriteExt;

#[tokio::main]
async fn main() -> io::Result<()> {
    let stream = TcpStream::connect("127.0.0.1:5555").await?;
    let mut writer = stream.slice_writes(6usize);

    writer.write_all(b"abcdefgh").await?; // forwarded as 6 bytes, then 2 bytes with flushes
    Ok(())
}
```

**Dynamic config example**

```rust,no_run
use std::sync::Arc;
use tokio::io::{self, AsyncWriteExt, BufWriter};
use tokio::net::TcpStream;
use tokio_netem::io::NetEmWriteExt;
use tokio_netem::slicer::DynamicSize;

#[tokio::main]
async fn main() -> io::Result<()> {
    let knob: Arc<DynamicSize> = DynamicSize::new(4);
    let stream = TcpStream::connect("127.0.0.1:5555").await?;
    let mut writer = BufWriter::new(stream).slice_writes_dyn(knob.clone());

    writer.write_all(b"12345678").await?; // 4-byte slices
    knob.set(2);
    writer.write_all(b"zzzz").await?; // now 2-byte slices
    Ok(())
}
```

### `ThrottledReader` & `ThrottledWriter`
Implements a leaky-bucket that meters bytes per second on reads and writes. Use `ThrottledReader` with
`BufReader` to restrict consumption of buffered data, and `ThrottledWriter` to pace outbound traffic.

**Static config example**

```rust,no_run
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_netem::io::{NetEmReadExt, NetEmWriteExt};

#[tokio::main]
async fn main() -> io::Result<()> {
    let stream = TcpStream::connect("127.0.0.1:5555").await?;

    let mut stream = stream.throttle_writes(32usize); // 32 B/s
    let mut stream = BufReader::new(stream).throttle_reads(64usize); // 64 B/s

    stream.write_all(b"ping").await?;
    stream.flush().await?;

    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await?; // limited by read throttle
    Ok(())
}
```

**Dynamic config example**

```rust,no_run
use std::sync::Arc;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_netem::io::{NetEmReadExt, NetEmWriteExt};
use tokio_netem::throttler::DynamicRate;

#[tokio::main]
async fn main() -> io::Result<()> {
    let rate: Arc<DynamicRate> = DynamicRate::new(0); // start unlimited
    let stream = TcpStream::connect("127.0.0.1:5555").await?;

    let mut stream = stream.throttle_writes_dyn(rate.clone());
    let mut stream = BufReader::new(stream).throttle_reads_dyn(rate.clone());

    stream.write_all(b"fast").await?;
    rate.set(8); // drop to 8 B/s for both halves
    stream.write_all(b"slow").await?;

    let mut buf = [0u8; 8];
    stream.read_exact(&mut buf).await?; // paced read completes slowly now

    Ok(())
}
```

### Corrupter

Randomly injects generated bytes according to a probability source.
Great for fuzzing higher-level protocols or asserting retry logic.

**Static config example**

```rust,no_run
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_netem::corrupter::Corrupter;

#[tokio::main]
async fn main() -> io::Result<()> {
    let stream = TcpStream::connect("127.0.0.1:5555").await?;

    let mut stream = Corrupter::new(stream, 0.25f64); // 25% chance per read or write poll

    stream.write_all(b"ping").await?; // possibly prepend random bytes if corruption fires
    let mut buf = vec![0u8; 4];
    let _ = stream.read(&mut buf).await?; // possibly random bytes if corruption fires
    Ok(())
}
```

**Dynamic config example**

```rust,no_run
use std::sync::Arc;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_netem::corrupter::Corrupter;
use tokio_netem::probability::DynamicProbability;

#[tokio::main]
async fn main() -> io::Result<()> {
    let knob: Arc<DynamicProbability> = DynamicProbability::new(0.0)?; // start disabled
    let stream = TcpStream::connect("127.0.0.1:5555").await?;
    let (reader_half, writer_half) = stream.into_split();

    let mut reader = Corrupter::new(reader_half, knob.clone());
    let mut writer = Corrupter::new(writer_half, knob.clone());

    writer.write_all(b"pristine").await?; // passes through while probability is 0
    knob.set(0.75)?; // now 75% chance to corrupt each poll

    let mut buf = vec![0u8; 8];
    match reader.read_exact(&mut buf).await {
        Ok(n) => println!("received {:?}", &buf[..n]),
        Err(err) => eprintln!("read failed: {err}"),
    }
    Ok(())
}
```

### `ReadInjector` & `WriteInjector`s

Inject synthetic frames into live streams by draining an `mpsc::Receiver<Bytes>` before polling the
inner I/O. Handy for replaying stale control messages or forging partial frames without touching the
main logic.

**Example**

```rust,no_run
use bytes::Bytes;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, duplex};
use tokio::sync::mpsc;
use tokio_netem::injector::{ReadInjector, WriteInjector};

#[tokio::main]
async fn main() -> io::Result<()> {
    let (reader_half, writer_half) = duplex(64);
    let (tx_read, rx_read) = mpsc::channel(4);
    let (tx_write, rx_write) = mpsc::channel(4);

    // Stage payloads before I/O starts.
    tx_read.send(Bytes::from_static(b"hello-")).await.unwrap();
    tx_write.send(Bytes::from_static(b"inject-")).await.unwrap();

    let mut reader = ReadInjector::new(reader_half, rx_read);
    let mut writer = WriteInjector::new(writer_half, rx_write);

    writer.write_all(b"payload").await?;
    writer.flush().await?;

    let mut buf = vec![0u8; 20];
    reader.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello-inject-payload");
    Ok(())
}
```

### `Terminator`

Randomly injects failures with a configured probability. Once tripped, the wrapper sticks in the
terminated state and all future I/O polls return `TERMINATED_ERROR`.

**Static config example**

```rust,no_run
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_netem::terminator::Terminator;

#[tokio::main]
async fn main() -> io::Result<()> {
    let stream = TcpStream::connect("127.0.0.1:5555").await?;
    let mut writer = Terminator::new(stream, 0.1f64);

    // Every poll has a 10% chance to fail and permanently terminate the stream.
    match writer.write_all(b"ping").await {
        Ok(()) => println!("write made it through"),
        Err(err) => println!("terminated early: {err}"),
    }
    Ok(())
}
```

**Dynamic config example**

```rust,no_run
use std::sync::Arc;
use tokio::io::{self, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_netem::terminator::{Terminator, TERMINATED_ERROR};
use tokio_netem::probability::DynamicProbability;

#[tokio::main]
async fn main() -> io::Result<()> {
    let probability: Arc<DynamicProbability> = DynamicProbability::new(0.0)?;
    let stream = TcpStream::connect("127.0.0.1:5555").await?;
    let mut writer = Terminator::new(stream, probability.clone());

    writer.write_all(b"once").await?; // safe while probability is 0
    probability.set(1.0)?; // guaranteed failure
    let err = writer.write_all(b"twice").await.unwrap_err();
    assert_eq!(err.to_string(), io::Error::other(TERMINATED_ERROR).to_string());
    Ok(())
}
```

### `Shutdowner`

Force-fail an I/O stream from the outside by sending an error through a `oneshot` channel. Ideal for
kill switches or simulating upstream aborts.

**Example**

```rust,no_run
use std::io;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_netem::shutdowner::Shutdowner;

#[tokio::main]
async fn main() -> io::Result<()> {
    let (tx, rx) = oneshot::channel();
    let stream = TcpStream::connect("127.0.0.1:5555").await?;
    let mut stream = Shutdowner::new(stream, rx);

    let killer = tokio::spawn(async move {
        let _ = tx.send(io::Error::new(io::ErrorKind::Other, "test harness stop").into());
    });

    let result = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
    killer.await.unwrap();
    assert!(result.is_err());
    Ok(())
}
```
