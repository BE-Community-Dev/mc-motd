# mc-motd

[English](README.md) | [简体中文](README.zh-CN.md)

`mc-motd` is a lightweight async Rust library for querying Minecraft server
status. It supports Java Edition status requests, Bedrock Edition through both
the legacy RakNet unconnected ping and the NetherNet join endpoint used by
Bedrock Edition 1.26.51+, Java SRV discovery, LAN discovery, and address
normalization for common server input formats. Editions are isolatable:
Bedrock-only builds can drop Java support and its DNS dependency entirely.

The crate intentionally keeps the API small: Java status JSON is returned raw so
applications can choose their own JSON model, while Bedrock MOTD fields are
parsed into typed optional values with zero-copy borrowed variants for hot
paths.

## Recommended API

- `query_minecraft_address(address, timeout)` is the easiest entry point. It
  accepts `host`, `host:port`, and `[ipv6]:port`. Java and Bedrock probes run
  concurrently and the first compatible response wins.
- `query_bedrock_address(address, timeout)` / `query_bedrock_auto(host, port,
  timeout)` are the Bedrock-only entry points: Java is never contacted, and the
  RakNet and NetherNet probes race with NetherNet preferred.
- `query_minecraft(host, port, timeout)` queries one explicit port across both
  editions concurrently and skips SRV discovery.
- `query_minecraft_with_options(host, options)` lets callers choose separate
  Java and Bedrock ports, timeout, SRV behavior, and `try_java` isolation.
- `query_java`, `query_bedrock`, and `query_nethernet` are edition/transport
  specific helpers.
- `query_bedrock_addr_into(address, buffer, timeout)` is the zero-allocation
  RakNet path: it reads the pong into a caller-owned buffer and returns a
  borrowed `BedrockMotdRef`.
- `discover_lan_servers(options)` finds Bedrock servers on the local network via
  UDP broadcast.
- `parse_java_status_response`, `parse_bedrock_pong`,
  `parse_bedrock_motd_text`, `parse_nethernet_join_json`, and
  `parse_nethernet_join_json_ref` are available for testing, proxies, and
  custom transports.

```rust
use std::time::Duration;
use mc_motd::{MinecraftMotd, query_minecraft_address};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let result = query_minecraft_address("play.example.com", Duration::from_secs(3)).await?;

    println!("transport={:?} latency={:?}", result.transport, result.latency);
    match result.motd {
        MinecraftMotd::Java(status) => println!("{}", status.raw_json),
        MinecraftMotd::Bedrock(motd) => println!("{} {}", motd.motd_line_1, motd.version),
        MinecraftMotd::BedrockNetherNet(motd) => println!("{} {}", motd.name, motd.version),
        // The enum is non_exhaustive so future editions can be added without
        // another breaking release.
        _ => {}
    }

    Ok(())
}
```

## Query Behavior

- Java Edition uses the status handshake and request packet over TCP.
- Bedrock Edition uses RakNet unconnected ping over UDP, the NetherNet join
  endpoint over HTTPS (see below), or both, depending on what the server
  answers.
- Automatic probing runs the Java and Bedrock probes concurrently: the host is
  resolved once and shared, both sides are polled from the start, and the first
  compatible response wins (Java is preferred on a tie; SRV targets, when
  present, stay authoritative for the Java side).
- Timeouts are caller-controlled and cover DNS lookup plus network I/O for the
  public query helpers.

## NetherNet (Bedrock Edition 1.26.51+)

Newer Bedrock servers advertise their status through a NetherNet join endpoint
instead of (or in addition to) the RakNet ping:
`GET https://{host}:{game-port}/v1/join` returns a JSON document with `name`,
`protocol`, `version`, `level`, `players`, `maxPlayers`, and `gameType`; any
`2xx` status means the server supports NetherNet. See the
[NetherNet onboarding guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/).

Automatic Bedrock probing:

- When the server transport is unknown, the RakNet probe and the NetherNet
  probe run concurrently and the faster transport wins (NetherNet is preferred
  on ties). Servers that answer both protocols always get the quickest answer.
- The transport that answers successfully is cached per `host:port`
  (`cached_transport`, `clear_transport_cache`) so later queries skip the
  failing probe. Entries expire after ten minutes.
- The HTTPS attempt happens on the game port; a plain `http://` endpoint is
  only tried after a transport-level failure (connection, TLS, or timeout),
  never after the server answered with an HTTP status.

Direct helpers: `query_nethernet(host, port, timeout)`,
`query_nethernet_with_options`, and `query_nethernet_url`. NetherNet queries
reuse a process-wide shared `reqwest::Client` (connection pooling, 5 second
connect timeout, no redirects), enforce a response body limit (64 KiB by
default), and validate the reported player counts and protocol field. The body
is parsed straight from the received bytes; `parse_nethernet_join_json_ref`
exposes the same parse with borrowed `Cow` fields for allocation-free polling.

Latency semantics: `latency` on the result is the round trip of the query
itself. For `Transport::NetherNetHttp` this is HTTPS signaling latency and is
**not** the game connection RTT, which requires a WebRTC handshake.

Verified live servers (ports are the game port 19132):
`play.enchanted.gg`, `nethernet.playhive.com`, and `mco.lbsg.net`.

## Isolating Editions

Bedrock-focused applications can drop Java entirely:

- Build with `default-features = false, features = ["nethernet"]` to compile
  only the Bedrock paths; the `java` feature (default-on) controls Java
  probing, SRV lookup, and the `hickory-resolver` dependency.
- Per call, `MotdQueryOptions::try_java = false` skips Java and SRV, or use the
  dedicated `query_bedrock_auto` / `query_bedrock_address` entry points.
- The zero-copy RakNet helper `query_bedrock_addr_into` reuses one caller-owned
  buffer across polls and returns a borrowed `BedrockMotdRef`, keeping
  steady-state allocation at zero for server list polling.

## LAN Discovery

`discover_lan_servers` broadcasts a RakNet unconnected ping on the local
network and collects unconnected pong replies. It is intentionally separate
from the public MOTD helpers: LAN discovery never contacts public NetherNet
HTTP endpoints, and public queries never broadcast on the LAN.

```rust
use std::time::Duration;
use mc_motd::{LanDiscoveryOptions, discover_lan_servers};

let servers = discover_lan_servers(LanDiscoveryOptions {
    listen_duration: Duration::from_secs(2),
    ..LanDiscoveryOptions::default()
}).await?;
```

The default broadcast port is 7551. Vanilla Bedrock LAN worlds advertise on
UDP 19132 (`VANILLA_LAN_PORT`); pass it via
`LanDiscoveryOptions::broadcast_port` to discover vanilla hosts. Replies are
deduped per source address and sorted; replies that fail to parse are ignored.

## Error Model

All fallible APIs return `MotdError`. It distinguishes invalid input, DNS
failures, network I/O, timeouts, invalid packets, invalid UTF-8, unexpected
HTTP statuses (`HttpStatus`), HTTP transport failures (`Http`), and the case
where both Java and Bedrock probing fail.

## Examples

```text
cargo run --example query -- play.example.com
cargo run --example query -- nethernet.playhive.com:19132
cargo run --example bedrock_only -- nethernet.playhive.com:19132
cargo run --example lan_discovery            # broadcast on UDP 7551
cargo run --example lan_discovery -- 19132   # vanilla Bedrock LAN port
```

## Tests

The test suite uses parser fixtures and local loopback mock servers. It does not
require public Minecraft servers. Live-server tests are marked `#[ignore]` and
can be run with `cargo test -- --ignored`.

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo package --allow-dirty
```

## Feature Flags

- `nethernet` (default): enables Bedrock NetherNet status queries through
  `reqwest` (rustls, webpki roots) plus `serde`/`serde_json`.
- `java` (default): enables Java Edition probing and SRV lookup
  (`hickory-resolver`).
- `default-features = false, features = ["nethernet"]` is the Bedrock-only
  build: no Java code, no SRV, no DNS resolver dependency.
- `default-features = false` is the legacy RakNet-only build with no HTTP
  dependencies at all.

## Current Limits

- Java status is exposed as raw JSON rather than a strongly typed schema.
- SRV lookup is Java Edition only, matching Minecraft's `_minecraft._tcp`
  convention.
- Automatic probing returns the first successful response per the preference
  order above; applications that need a specific edition or transport should
  call `query_java`, `query_bedrock`, or `query_nethernet` explicitly.
- NetherNet latency reflects HTTP signaling, not the WebRTC game path.
- LAN discovery sends one broadcast per call from an IPv4 socket and does not
  subscribe to periodic LAN advertisements.
