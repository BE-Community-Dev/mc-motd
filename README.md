# mc-motd

[English](README.md) | [简体中文](README.zh-CN.md)

`mc-motd` is a lightweight async Rust library for querying Minecraft server
status. It supports Java Edition status requests, Bedrock Edition through both
the legacy RakNet unconnected ping and the NetherNet join endpoint used by
Bedrock Edition 1.26.51+, Java SRV discovery, LAN discovery, and address
normalization for common server input formats.

The public API is organized in three layers:

1. **One-liner queries** — `mc_motd::query(address)` probes every compiled
   edition concurrently and returns the first compatible response.
2. **Reusable clients** — `Client` shares the Java SRV resolver, the
   NetherNet connection pool, and a bounded transport cache across queries.
3. **Protocol codecs** — `mc_motd::protocol` exposes pure encode/decode helpers
   (no sockets, no caching), including zero-copy parsing paths.

## Getting started

```rust
use mc_motd::{Details, query};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let status = query("play.example.com").await?;

    println!("transport={:?} elapsed={:?}", status.transport(), status.elapsed);
    println!("{}", status.motd);
    println!("{:?}", status.players);
    match &status.details {
        Details::Java(details) => println!("{}", details.raw_json),
        Details::Bedrock(bedrock) => println!("{bedrock:?}"),
        // Non_exhaustive: future protocols fit without another breaking release.
        _ => {}
    }

    Ok(())
}
```

## Query paths

| Call                        | Behavior                                                                  |
| --------------------------- | ------------------------------------------------------------------------- |
| `mc_motd::query(address)`   | Probe every compiled edition concurrently; first response wins.            |
| `mc_motd::java::query`      | Java only: SRV records first, then the direct port.                        |
| `mc_motd::bedrock::query`   | Bedrock only: RakNet and NetherNet race; faster transport wins.            |
| `bedrock::raknet::query`    | Force the legacy RakNet UDP ping.                                          |
| `bedrock::nethernet::query` | Force the NetherNet join endpoint (`GET /v1/join` over HTTPS).             |
| `lan::discover(options)`    | Broadcast on the local network; never contacts public servers.             |

Forced-transport queries surface transport errors directly; automatic paths
aggregate per-probe failures into structured `Error::NoCompatible*` values with
`Attempt { transport, target, error }` entries, so callers can still traverse
the original error chain.

## Client

```rust
use std::time::Duration;
use mc_motd::Client;

let client = Client::builder()
    .timeout(Duration::from_secs(3))
    .cache_capacity(1024)
    .allow_insecure_http(false)
    .build();

let status = client.query("play.example.com").await?;
let bedrock = client.bedrock("be.example.com").await?;
let java = client.java("mc.example.com").await?;
```

`Client` is cheap to clone and shares its resources across tasks. For
one-off configuration use `Client::query_with(address, QueryOptions)` — the
chainable options are applied **on top of** the client defaults, so a per-query
`timeout` keeps the builder's ports and policies:

```rust
use std::time::Duration;
use mc_motd::{Client, Edition, QueryOptions};

let client = Client::default();
let status = client
    .query_with(
        "play.example.com",
        QueryOptions::default()
            .timeout(Duration::from_secs(2))
            .edition(Edition::Bedrock),
    )
    .await?;
```

## Unified status model

Every successful query returns `Status`:

- `address` — the parsed server address that was queried.
- `motd` — normalized first line: Java description text, RakNet first MOTD
  line, or the NetherNet server name.
- `players` / `version` — normalized counts and version info.
- `elapsed` — end-to-end wall time of the whole query (DNS + probes). For
  NetherNet this is HTTPS signaling latency and **not** the game connection
  RTT, which requires a WebRTC handshake.
- `details` — protocol-specific raw payload: Java raw JSON (plus the SRV
  target when used), RakNet pong fields, or the NetherNet join response
  (`nonce`, `onlineAuth`, `transportLayer`, ...). `edition()` and `transport()`
  are derived from `details`, so there is no second stored copy to drift.

## Query behavior

- Automatic probing polls Java and Bedrock probes in parallel and both sides
  start immediately: Java handles SRV (budget-capped to one second) and its
  DNS resolution inside its own probe, and the Bedrock race starts with
  NetherNet on its own resolver stack while RakNet resolves lazily. No serial
  SRV/DNS phase delays a transport that is ready to answer. The first
  compatible response wins and Java is preferred on a tie; SRV records, when
  present, stay authoritative for the Java side.
- Inside the Bedrock step the RakNet and NetherNet probes race concurrently;
  the faster transport wins and NetherNet is preferred on ties. A cached
  transport is tried first, and when the cached transport plus the fallback
  both fail, the aggregate error keeps both probe failures.
- The transport that answers is cached per `host:port` inside the calling
  `Client` (10 minute TTL, bounded capacity) so later queries skip the failing
  probe. A cache hit is waited on through a short grace window; if it has not
  answered by then (a server switched transports), the other transport joins
  the race as a parallel fallback. `client.cached_transport()` and
  `client.clear_transport_cache()` expose the cache; the recorded transport
  distinguishes `NetherNetHttps` from `NetherNetHttp`.
- `timeout` is an end-to-end deadline: SRV lookup, DNS, and every probe share
  it instead of restarting it per stage.
- Direct Java probes — and every RakNet transport path — try every resolved
  address (IPv4 first) within the shared deadline before failing.
- NetherNet HTTPS runs on the game port; a plain `http://` endpoint is only
  tried after a transport-level failure and when the insecure-HTTP policy
  allows it (`allow_insecure_http`, on by default). Receiving any HTTP status
  never triggers the fallback.
- SRV records are ordered by priority with RFC 2782 weighted-random selection
  inside each priority class; zero-weight records own an empty range and are
  only ordered once nothing else remains. An explicitly specified port skips
  SRV entirely.
- A status response whose JSON cannot be parsed — or is not a JSON object —
  is a protocol failure (`Error::InvalidPacket`), not a blank status; missing
  optional fields stay lenient. The JSON is parsed once per query.
- SRV records pointing at `.` or port 0 mark the service decidedly
  unavailable and surface as an attempt error instead of silently falling
  back to direct probing.
- `allow_insecure_http` defaults to **disabled**: plain-HTTP NetherNet answers
  are unauthenticated, so the fallback must be enabled explicitly per client
  builder or per query. `Transport::NetherNetHttp` on a status marks an
  insecure-HTTP answer; `Transport::NetherNetHttps` marks a verified one.
- LAN discovery dedupes replies per socket address (multiple servers on one
  host are all reported), caps results (`max_results`, default 32), and
  correlates pongs with the ping timestamp to ignore stale replies.
- `unsafe_code` is forbidden crate-wide, and CI runs `cargo audit` plus a
  Windows job alongside the Linux feature matrix.

## Protocol layer

```text
mc_motd::protocol::java::encode_status_request(...)
mc_motd::protocol::java::parse_status_response(...)
mc_motd::protocol::bedrock::raknet::encode_ping()
mc_motd::protocol::bedrock::raknet::decode_pong(...)
mc_motd::protocol::bedrock::nethernet::parse_join(...)
```

Zero-copy paths:

- `bedrock::raknet::query_addr_into(address, buffer, timeout)` reads the pong
  into a caller-owned buffer and returns a borrowed `RakNetDetailsRef`;
  reusing one buffer across polls keeps steady-state explicit allocation at
  zero (runtime internals such as socket creation aside). The whole exchange
  — connect, send, receive — shares one deadline.
- `protocol::bedrock::raknet::decode_pong` and `decode_motd_text` parse into
  borrowed `Cow` fields with no heap allocation at all: a first MOTD line
  containing semicolons is one contiguous byte span of the input and is
  borrowed as a whole.
- `protocol::bedrock::nethernet::parse_join_ref` deserializes the join JSON
  into borrowed `Cow` fields with `#[serde(borrow)]`.

Verified live NetherNet servers (ports are the game port 19132):
`play.enchanted.gg`, `nethernet.playhive.com`, and `mco.lbsg.net`.

## Feature flags

| Feature     | Default | Enables                                                            |
| ----------- | ------- | ------------------------------------------------------------------ |
| `java`      | yes     | Java probing, SRV lookup (`hickory-resolver`), JSON status parsing. |
| `bedrock`   | yes     | RakNet probing and the unified Bedrock status model.                |
| `nethernet` | yes     | NetherNet HTTPS queries (`reqwest` + rustls, implies `bedrock`).    |
| `lan`       | no      | LAN discovery broadcasts (`socket2`, implies `bedrock`).            |

- Bedrock-only build: `default-features = false, features = ["nethernet"]` —
  no Java code, no SRV, no DNS resolver dependency.
- Legacy RakNet-only build: `default-features = false, features = ["bedrock"]`
  — no HTTP or TLS dependencies at all.

## Examples

```text
cargo run --example query -- play.example.com
cargo run --example bedrock_only -- nethernet.playhive.com:19132
cargo run --example lan_discovery --features lan            # broadcast on UDP 7551
cargo run --example lan_discovery --features lan -- 19132   # vanilla Bedrock LAN port
```

## Tests

The test suite uses parser fixtures and local loopback mock servers. It does
not require public Minecraft servers. Live-server tests are marked `#[ignore]`:

```text
cargo test
cargo test --features lan -- --ignored   # includes LAN broadcast
```

Quality gates:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo package --allow-dirty
```

## Current limits

- Java status is parsed leniently for the normalized fields; the raw JSON
  stays available in `JavaDetails` for full component-tree rendering.
- SRV lookup is Java Edition only, following Minecraft's `_minecraft._tcp`
  convention.
- Automatic probing returns the first successful response per the preference
  order above; applications that need a specific edition or transport should
  call `java::query`, `bedrock::raknet::query`, or `bedrock::nethernet::query`.
- NetherNet latency reflects HTTP signaling, not the WebRTC game path.
- LAN discovery sends one broadcast per call from an IPv4 socket and does not
  subscribe to periodic LAN advertisements.
