# mc-motd

[English](README.md) | [简体中文](README.zh-CN.md)

`mc-motd` is a lightweight async Rust library for querying Minecraft server
status. It supports Java Edition status requests, Bedrock Edition RakNet
unconnected pings, Java SRV discovery, and address normalization for common
server input formats.

The crate intentionally keeps the API small: Java status JSON is returned raw so
applications can choose their own JSON model, while Bedrock MOTD fields are
parsed into typed optional values.

## Recommended API

- `query_minecraft_address(address, timeout)` is the easiest entry point. It
  accepts `host`, `host:port`, and `[ipv6]:port`. When no port is present, the
  library automatically uses the Java and Bedrock default ports.
- `query_minecraft(host, port, timeout)` queries one explicit port and skips SRV
  discovery.
- `query_minecraft_with_options(host, options)` lets callers choose separate
  Java and Bedrock ports, timeout, and SRV behavior.
- `query_java` and `query_bedrock` are edition-specific helpers.
- `parse_java_status_response`, `parse_bedrock_pong`, and
  `parse_bedrock_motd_text` are available for testing, proxies, and custom
  transports.

```rust
use std::time::Duration;
use mc_motd::{MinecraftMotd, query_minecraft_address};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let result = query_minecraft_address("play.example.com", Duration::from_secs(3)).await?;

    match result.motd {
        MinecraftMotd::Java(status) => println!("{}", status.raw_json),
        MinecraftMotd::Bedrock(motd) => println!("{} {}", motd.motd_line_1, motd.version),
    }

    Ok(())
}
```

## Query Behavior

- Java Edition uses the status handshake and request packet over TCP.
- Bedrock Edition uses RakNet unconnected ping over UDP.
- Address queries try Java SRV records first when the input does not include an
  explicit port and the host is a DNS name.
- Direct probing tries Java first, then Bedrock, and returns the first compatible
  response.
- Timeouts are caller-controlled and cover DNS lookup plus network I/O for the
  public query helpers.

## Error Model

All fallible APIs return `MotdError`. It distinguishes invalid input, DNS
failures, network I/O, timeouts, invalid packets, invalid UTF-8, and the case
where both Java and Bedrock probing fail.

## Examples

```text
cargo run --example query -- play.example.com
cargo run --example query -- play.example.com:25565
```

## Tests

The test suite uses parser fixtures and local loopback mock servers. It does not
require public Minecraft servers.

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo package --allow-dirty
```

## Current Limits

- Java status is exposed as raw JSON rather than a strongly typed schema.
- SRV lookup is Java Edition only, matching Minecraft's `_minecraft._tcp`
  convention.
- Automatic probing returns the first successful edition response; applications
  that need both editions should call `query_java` and `query_bedrock`
  explicitly.
