# Project Overview

- [`trixter`](#trixter--chaos-monkey-tcp-proxy) — a high‑performance, runtime‑tunable TCP chaos proxy — a minimal, blazing‑fast written in Rust with **Tokio**. It lets you inject latency, throttle bandwidth, slice writes (to simulate small MTUs/Nagle‑like behavior), corrupt bytes in flight by injecting random bytes, randomly terminate connections, and hard‑timeout sessions – all controllable per connection via a simple REST API.

- [`tokio-netem`](tokio-netem/README.md) — a collection of Tokio `AsyncRead`/`AsyncWrite` adapters (delay, throttle, slice, terminate, shutdown, corrupt data, inject data) that power the `Trixter` proxy and can be used independently in tests and harnesses. [![Crates.io][crates-badge]][crates-url]

The remainder of this document dives into the proxy. For the adapter crate’s detailed guide, follow the `tokio-netem` link above.

[![MIT licensed][mit-badge]][mit-url]

[crates-badge]: https://img.shields.io/crates/v/tokio-netem.svg
[crates-url]: https://crates.io/crates/tokio-netem
[mit-badge]: https://img.shields.io/badge/license-MIT-blue.svg
[mit-url]: https://github.com/brk0v/trixter/blob/master/LICENSE

---

# Trixter – Chaos Monkey TCP Proxy

A high‑performance, runtime‑tunable TCP chaos proxy — a minimal, blazing‑fast alternative to [Toxiproxy](https://github.com/Shopify/toxiproxy) written in Rust with **Tokio**. It lets you inject latency, throttle bandwidth, slice writes (to simulate small MTUs/Nagle‑like behavior), corrupt bytes in flight by injecting random bytes, randomly terminate connections, and hard‑timeout sessions – all controllable per connection via a simple REST API.

---

## Why Trixter?
- **Zero-friction**: one static binary, no external deps.
- **Runtime knobs**: flip chaos on/off without restarting.
- **Per-conn control**: target just the flows you want.
- **Minimal overhead**: adapters are lightweight and composable.

## Features

* **Fast path**: `tokio::io::copy_bidirectional` on a multi‑thread runtime;
* **Runtime control** (per active connection):
  * **Latency**: add/remove delay in ms.
  * **Throttle**: cap bytes/sec.
  * **Slice**: split writes into fixed‑size chunks.
  * **Corrupt**: inject random bytes with a tunable probability.
  * **Chaos termination**: probability \[0.0..=1.0] to abort on each read/write.
  * **Hard timeout**: stop a session after N milliseconds.
* **REST API** to list connections and change settings on the fly.
* **Targeted kill**: shut down a single connection with a reason.
* **RST on chaos**: resets (best‑effort) when a timeout/termination triggers.

---

## Quick start

### 1. Run an upstream echo server (demo)

Use any TCP server. Examples:

```bash
nc -lk 127.0.0.1 8181
```

### 2. Run `trixter` chaos proxy

with `docker`:

```bash
docker run --network host -it --rm ghcr.io/brk0v/trixter \
    --listen 0.0.0.0:8080 \
    --upstream 127.0.0.1:8181 \
    --api 127.0.0.1:8888 \
    --delay-ms 0 \
    --throttle-rate-bytes 0 \
    --slice-size-bytes 0 \
    --corrupt-probability-rate 0.0 \
    --terminate-probability-rate 0.0 \
    --connection-duration-ms 0
```

or build from scratch:

```bash
cd trixter/trixter
cargo build --release
```

or install with `cargo`:

```bash
cargo install trixter
```

and run:

```bash
RUST_LOG=info \
./target/release/trixter \
  --listen 0.0.0.0:8080 \
  --upstream 127.0.0.1:8181 \
  --api 127.0.0.1:8888 \
  --delay-ms 0 \
  --throttle-rate-bytes 0 \
  --slice-size-bytes 0 \
  --corrupt-probability-rate 0.0 \
  --terminate-probability-rate 0.0 \
  --connection-duration-ms 0
```

### 3. Test

Now connect your app/CLI to `localhost:8080`. The proxy forwards to `127.0.0.1:8181`.

---

## REST API

Base URL is the `--api` address, e.g. `http://127.0.0.1:8888`.

### Data model

```json
{
  "conn_info": {
    "id": "pN7e3y...",
    "downstream": "127.0.0.1:59024",
    "upstream": "127.0.0.1:8181"
  },
  "delay": { "secs": 2, "nanos": 500000000 },
  "throttle_rate": 10240,
  "slice_size": 512,
  "terminate_probability_rate": 0.05,
  "corrupt_probability_rate": 0.02
}
```

Notes:

* `delay` serializes as a [`std::time::Duration`](https://docs.rs/serde/latest/serde/ser/trait.Serializer.html) object with `secs`/`nanos` fields (zeroed when the delay is disabled).
* `id` is unique per connection; use it to target a single connection.
* `corrupt_probability_rate` reports the current per-operation flip probability (`0.0` when corruption is off).

### Health check

```bash
curl -s http://127.0.0.1:8888/health
```

### List connections

```bash
curl -s http://127.0.0.1:8888/connections | jq
```

### Kill a connection

```bash
ID=$(curl -s http://127.0.0.1:8888/connections | jq -r '.[0].conn_info.id')

curl -i -X POST \
  http://127.0.0.1:8888/connections/$ID/shutdown \
  -H 'Content-Type: application/json' \
  -d '{"reason":"test teardown"}'
```

### Kill all connections

```bash
curl -i -X POST \
  http://127.0.0.1:8888/connections/_all/shutdown \
  -H 'Content-Type: application/json' \
  -d '{"reason":"test teardown"}'
```

### Set latency (ms)

```bash
curl -i -X PATCH \
  http://127.0.0.1:8888/connections/$ID/delay \
  -H 'Content-Type: application/json' \
  -d '{"delay_ms":250}'

# Remove latency
curl -i -X PATCH \
  http://127.0.0.1:8888/connections/$ID/delay \
  -H 'Content-Type: application/json' \
  -d '{"delay_ms":0}'
```

### Throttle bytes/sec

```bash
curl -i -X PATCH \
  http://127.0.0.1:8888/connections/$ID/throttle \
  -H 'Content-Type: application/json' \
  -d '{"rate_bytes":10240}'  # 10 KiB/s
```

### Slice writes (bytes)

```bash
curl -i -X PATCH \
  http://127.0.0.1:8888/connections/$ID/slice \
  -H 'Content-Type: application/json' \
  -d '{"size_bytes":512}'
```

### Randomly terminate reads/writes

```bash
# Set 5% probability per read/write operation
curl -i -X PATCH \
  http://127.0.0.1:8888/connections/$ID/termination \
  -H 'Content-Type: application/json' \
  -d '{"probability_rate":0.05}'
```

### Inject random bytes

```bash
# Corrupt ~1% of operations
curl -i -X PATCH \
  http://127.0.0.1:8888/connections/$ID/corruption \
  -H 'Content-Type: application/json' \
  -d '{"probability_rate":0.01}'

# Remove corruption
curl -i -X PATCH \
  http://127.0.0.1:8888/connections/$ID/corruption \
  -H 'Content-Type: application/json' \
  -d '{"probability_rate":0.0}'
```

### Error responses

* `404 Not Found` — bad connection ID
* `400 Bad Request` — invalid probability (outside 0.0..=1.0) for termination/corruption
* `500 Internal Server Error` — internal channel/handler error

---

## CLI flags

```
--listen <ip:port>                  # e.g. 0.0.0.0:8080
--upstream <ip:port>                # e.g. 127.0.0.1:8181
--api <ip:port>                     # e.g. 127.0.0.1:8888
--delay-ms <ms>                     # 0 = off (default)
--throttle-rate-bytes <bytes/s>     # 0 = unlimited (default)
--slice-size-bytes <bytes>          # 0 = off (default)
--terminate-probability-rate <0..1> # 0.0 = off (default)
--corrupt-probability-rate <0..1>   # 0.0 = off (default)
--connection-duration-ms <ms>       # 0 = unlimited (default)
```

> All of the above can be changed **per connection** at runtime via the REST API, except `--connection-duration-ms` which is a process‑wide default applied to new connections.

---

## How it works (architecture)

Each accepted downstream connection spawns a task that:

1. Connects to the upstream target.
2. Wraps both sides with tunable adapters with `tokio-netem`:

   * `DelayedWriter` → optional latency
   * `ThrottledWriter` → bandwidth cap
   * `SlicedWriter` → fixed‑size write chunks
   * `Terminator` → probabilistic aborts
   * `Corrupter` → probabilistic random byte injector
   * `Shutdowner` (downstream only) → out‑of‑band shutdown via oneshot channel
3. Runs `tokio::io::copy_bidirectional` until EOF/error/timeout.
4. Tracks the live connection in a `DashMap` so the API can query/mutate it.

---

## Use cases

* **Flaky networks**: simulate 3G/EDGE/satellite latency and low bandwidth.
* **MTU/segmentation bugs**: force small write slices to uncover packetization assumptions.
* **Resilience drills**: randomly kill connections during critical paths.
* **Data validation**: corrupt bytes to exercise checksums and retry logic.
* **Timeout tuning**: enforce hard upper‑bounds to validate client retry/backoff logic.
* **Canary/E2E tests**: target only specific connections and tweak dynamically.
* **Load/soak**: run for hours with varying chaos settings from CI/scripts.

---

## Recipes

### Simulate a shaky mobile link

```bash
# Add ~250ms latency and 64 KiB/s cap to the first active connection
ID=$(curl -s localhost:8888/connections | jq -r '.[0].conn_info.id')

curl -s -X PATCH localhost:8888/connections/$ID/delay    \
  -H 'Content-Type: application/json' -d '{"delay_ms":250}'

curl -s -X PATCH localhost:8888/connections/$ID/throttle \
  -H 'Content-Type: application/json' -d '{"rate_bytes":65536}'
```

### Force tiny packets (find buffering bugs)

```bash
curl -s -X PATCH localhost:8888/connections/$ID/slice \
  -H 'Content-Type: application/json' -d '{"size_bytes":256}'
```

### Introduce flakiness (5% ops abort)

```bash
curl -s -X PATCH localhost:8888/connections/$ID/termination \
  -H 'Content-Type: application/json' -d '{"probability_rate":0.05}'
```

### Add data corruption

```bash
curl -s -X PATCH localhost:8888/connections/$ID/corruption \
  -H 'Content-Type: application/json' -d '{"probability_rate":0.01}'
```

### Timebox a connection to 5s at startup

```bash
./trixter \
  --listen 0.0.0.0:8080 \
  --upstream 127.0.0.1:8181 \
  --api 127.0.0.1:8888 \
  --connection-duration-ms 5000
```

### Kill the slowpoke

```bash
curl -s -X POST localhost:8888/connections/$ID/shutdown \
  -H 'Content-Type: application/json' -d '{"reason":"too slow"}'
```

---

## Integration: CI & E2E tests

* Spin up the proxy as a sidecar/container.
* Discover the right connection (by `downstream`/`upstream` pair) via `GET /connections`.
* Apply chaos during specific test phases with `PATCH` calls.
* Always clean up with `POST /connections/{id}/shutdown` to free ports quickly.

---

## Performance notes

* Built on `Tokio` multi‑thread runtime; avoid heavy CPU work on the I/O threads.
* Throttling and slicing affect throughput by design; set them to `0` to disable.
* Use loopback or a fast NIC for local tests; network stack/OS settings still apply.
* Logging: `RUST_LOG=info` (or `debug`) for visibility; turn off for max throughput.

---

## Security

* The API performs no auth; **bind it to a trusted interface** (e.g., `127.0.0.1`).
* The proxy is transparent TCP; apply your own TLS/ACLs at the edges if needed.

---

## Error handling

* Invalid probability returns `400` with `{ "error": "invalid probability; must be between 0.0 and 1.0" }`.
* Unknown connection IDs return `404`.
* Internal channel/handler errors return `500`.

---

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
        let _ = tx.send(io::Error::other("test harness stop").into());
    });

    let result = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
    killer.await.unwrap();
    assert!(result.is_err());
    Ok(())
}
```

---

## License

MIT
