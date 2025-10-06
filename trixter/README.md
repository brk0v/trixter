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

## License

MIT
