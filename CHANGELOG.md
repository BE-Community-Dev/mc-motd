# Changelog

All notable changes to this project will be documented in this file.

## 0.2.0 - Unreleased

### Added

- Bedrock Edition `NetherNet` support (Bedrock 1.26.51+): automatic queries fetch
  the `GET /v1/join` status endpoint over HTTPS on the game port and only fall
  back to plain HTTP after transport-level failures, following the
  [NetherNet onboarding guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/).
- `MinecraftMotd::BedrockNetherNet` variant with typed join response fields
  (`name`, `protocol`, `version`, `level`, player counts, `gameType`, and the
  extensions servers report such as `nonce` and `onlineAuth`).
- `Transport` (`JavaTcp` / `RakNetUdp` / `NetherNetHttp`) and `latency` on
  `MinecraftMotdResult`. `NetherNet` latency is HTTP signaling time and is not
  the game connection RTT.
- Concurrent automatic probing: Java and Bedrock probes are polled in parallel
  with a single shared DNS resolution, the first compatible response wins, and
  inside the Bedrock race the faster transport wins (`NetherNet` preferred on
  ties). A process-wide cache of the transport that last answered
  (`cached_transport`, `clear_transport_cache`) lets repeat queries skip the
  failing probe.
- Bedrock-only entry points `query_bedrock_auto` and `query_bedrock_address`,
  plus `MotdQueryOptions::try_java`, so Bedrock-focused callers never contact
  Java. The `java` cargo feature (default-on) removes Java probing, SRV lookup,
  and the `hickory-resolver` dependency when disabled.
- Zero-copy paths: `query_bedrock_addr_into` reads the RakNet pong into a
  caller-owned buffer and returns a borrowed `BedrockMotdRef`;
  `parse_nethernet_join_json_ref` parses the `NetherNet` JSON into borrowed
  `Cow` fields without per-field allocation.
- `query_nethernet`, `query_nethernet_with_options`, `query_nethernet_url`, and
  `parse_nethernet_join_json` for direct `NetherNet` queries with a shared
  connection pool, per-attempt timeouts, and a 64 KiB response body limit.
- LAN discovery (`discover_lan_servers`, `LanDiscoveryOptions`) as a separate
  UDP broadcast flow that never mixes with public MOTD queries. The default
  broadcast port is 7551; vanilla LAN hosts use 19132 (`VANILLA_LAN_PORT`).
- `MotdError::HttpStatus`, `MotdError::Http`, and
  `MotdError::NoCompatibleBedrockServer` for HTTP-level and Bedrock-only
  failures.

### Changed

- **Breaking:** `MinecraftMotd` and `MinecraftMotdResult` are now
  `#[non_exhaustive]`, and `MinecraftMotdResult` gains `transport` and
  `latency` fields. Matches on `MinecraftMotd` need a wildcard arm.
- **Breaking:** `MotdQueryOptions` gains `try_java`; automatic probing is now
  concurrent instead of Java-then-Bedrock sequential.
- **Breaking:** `NetherNet` support ships as the default `nethernet` cargo
  feature, which pulls in `reqwest` (rustls) plus `serde`/`serde_json`. Build
  with `default-features = false` to keep the RakNet-only dependency set.

## 0.1.0 - Unreleased

- Initial async Minecraft MOTD query library.
- Supports Java Edition status requests and Bedrock Edition RakNet unconnected pings.
- Supports server address normalization, explicit ports, Java SRV lookup, and automatic Java-then-Bedrock probing.
