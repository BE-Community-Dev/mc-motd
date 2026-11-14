# Changelog

All notable changes to this project will be documented in this file.

## 0.2.0 - Unreleased

### Added

- Bedrock Edition `NetherNet` support (Bedrock 1.26.51+): automatic queries fetch
  the `GET /v1/join` status endpoint over HTTPS on the game port and only fall
  back to plain HTTP after transport-level failures, following the
  [NetherNet onboarding guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/).
- Three-layer public API:
  - one-liner queries (`mc_motd::query`, `java::query`, `bedrock::query`,
    `bedrock::raknet::query`, `bedrock::nethernet::query`, `lan::discover`);
  - a reusable, cheap-to-clone `Client` with a builder (timeout, cache capacity,
    insecure-HTTP policy) and `query_with` + chainable `QueryOptions`;
  - a pure `protocol` module for encode/decode with zero-copy paths.
- Unified `Status` result model (`address`, normalized `motd`, `players`,
  `version`, end-to-end `elapsed`, protocol `details`) shared by Java, `RakNet`,
  and `NetherNet`; edition and transport are derived from `details`.
- Structured error model: `Error` with `Attempt { transport, target, error }`
  entries preserving the original error chain, plus
  `NoCompatibleServer` / `NoCompatibleJavaServer` / `NoCompatibleBedrockServer`.
- Concurrent automatic probing: Java and Bedrock probes are polled in parallel
  with a single shared DNS resolution, the first compatible response wins, and
  inside the Bedrock race the faster transport wins (`NetherNet` preferred on
  ties). A per-`Client` bounded cache of the transport that last answered
  (`cached_transport`, `clear_transport_cache`) lets repeat queries skip the
  failing probe.
- End-to-end query deadlines: SRV lookup, DNS, and every probe share one
  deadline instead of restarting a per-stage timeout.
- Direct Java probes try every resolved address (IPv4 first) before failing.
- SRV records are ordered with RFC 2782 weighted-random selection inside each
  priority class.
- LAN discovery (`lan::discover`, `lan::DiscoveryOptions`) as a separate UDP
  broadcast flow that never mixes with public queries. The default broadcast
  port is 7551; vanilla LAN hosts use 19132 (`lan::VANILLA_PORT`).
- Zero-copy paths: `bedrock::raknet::query_addr_into` reads the RakNet pong
  into a caller-owned buffer and returns a borrowed `RakNetDetailsRef`;
  `protocol::bedrock::nethernet::parse_join_ref` parses the join JSON into
  borrowed `Cow` fields. The MOTD text decoder now allocates nothing on its
  common path.
- HTTP policy as explicit configuration: `allow_insecure_http` on
  `QueryOptions` and the client builder.
- Feature matrix: `java`, `bedrock`, `nethernet` (default), `lan` (opt-in) so
  Bedrock-only builds drop Java, SRV, and the DNS resolver dependency, and
  RakNet-only builds drop HTTP/TLS entirely.

### Changed

- **Breaking:** full API restructure around `库::领域::操作` module paths
  instead of implementation-named root functions. Key mappings:
  `query_minecraft_address` → `query`; `query_bedrock_address` /
  `query_bedrock_auto` → `bedrock::query`; `query_bedrock` →
  `bedrock::raknet::query`; `query_nethernet` → `bedrock::nethernet::query`;
  `discover_lan_servers` → `lan::discover`; `parse_bedrock_pong` →
  `protocol::bedrock::raknet::decode_pong`; `MinecraftMotdResult` → `Status`;
  `MotdError` → `Error`; `MotdQueryOptions` → `QueryOptions`. The old functions
  were removed rather than wrapped.
- **Breaking:** `Status` normalizes `motd`, players, and version across
  transports; transport-specific payloads (NetherNet `nonce`/`onlineAuth`,
  RakNet server id, Java raw JSON) moved into `Details`. `latency` fields were
  removed from detail structs in favor of the top-level end-to-end `elapsed`.
- **Breaking:** automatic probing is concurrent instead of Java-then-Bedrock
  sequential, and the Bedrock race prefers the fastest transport.
- **Breaking:** `timeout` is an end-to-end deadline rather than a per-stage
  timeout.
- **Breaking:** the transport cache moved from a process global into
  `Client` instances.

## 0.1.0 - Unreleased

- Initial async Minecraft MOTD query library.
- Supports Java Edition status requests and Bedrock Edition RakNet unconnected pings.
- Supports server address normalization, explicit ports, Java SRV lookup, and automatic Java-then-Bedrock probing.
