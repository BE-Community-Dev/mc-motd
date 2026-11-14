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
- Security and scheduling hardening from review:
  - `allow_insecure_http` defaults to **disabled**; the actual scheme used is
    reported through `Transport::NetherNetHttps` / `Transport::NetherNetHttp`.
  - Multi-address failover gives each candidate a fair share of the shared
    deadline, and a cached transport hit is waited on through a short grace
    window before the other transport joins the race in parallel.
  - Java status JSON is parsed once into a typed structure that validates the
    object shape; non-object JSON fails with `Error::InvalidPacket`.
  - The Java SRV resolver is reused per `Client`; SRV records pointing at `.`
    surface as an explicit "service unavailable" attempt.
  - LAN discovery dedupes per socket address, caps results, and correlates
    pongs with the ping timestamp; `unsafe_code` is forbidden crate-wide;
    CI gained Windows, a feature matrix, and `cargo audit`.
- Performance and scheduling fixes from review:
  - the HTTP cache-hit branch now checks `allow_insecure_http` before using a
    cached plain-HTTP transport, and a dedicated `NetherNetHttps` cache-hit
    branch makes HTTPS caching effective
  - pure fair-share address budgets replace the 250ms floor; SRV targets get
    their own budget layer
  - RakNet lazy-resolution path no longer allocates an unused 2048-byte
    buffer; Java description flattening appends into one string; HTTP body
    reserve is capped at `min(Content-Length, 4 KiB)` and grows as data
    arrives instead of trusting the remote hint
  - `TransportCache` keys are a true single allocation (in-place ASCII
    lowercasing plus `fmt::Write` for the port); `Cow` docs now note that
    escaped JSON strings still allocate
  - the singleflight key is built from the parsed target ports, the
    explicit-port flag, and the timeout, so same-host/different-port queries
    can never merge or return each other's status
  - singleflight uses a `watch` channel instead of `Notify`, so a shared probe
    that completes before a waiter starts waiting is still observed; only the
    owning task removes the map entry (guarded by `Arc::ptr_eq`) and waiters
    share failures instead of re-probing
  - `max_in_flight` is acquired at every engine entry point with exactly one
    permit per logical query; the nested `full -> bedrock -> raknet` layers
    reuse it, and zero or out-of-range limits are rejected at build time
  - Java SRV lookup returns a typed outcome (`Targets` / `NoRecords` /
    `Unavailable`) instead of inspecting error message text
  - a `FlightGuard` is armed before the first admission await and captured by
    the detached probe task, so cancellation during admission, abort, panic, or
    runtime shutdown all remove the flight and publish an error; its message
    is neutral because `Drop` cannot distinguish those cases
  - `max_queued(n)` bounds admitted merged probes separately from
    `max_in_flight`; the pending flight is inserted before waiting for a queue
    slot, so a saturated queue still merges same-key callers into one probe
    instead of re-probing per released slot
  - Java description flattening appends every array element and `extra` child
    into one output buffer; aggregate error formatting writes directly into the
    formatter instead of building a `Vec<String>`
  - RFC 2782 weighted selection samples the inclusive `[0, sum]` range (so
    zero-weight records keep their small chance) and shuffles all-zero groups
    instead of always returning the first record; the random source is
    injectable and the distribution tests use fixed seeds, so CI cannot flake
    on clock entropy
- Contract fixes from review: an explicitly specified port skips SRV entirely;
  `Status.address` keeps the requested address (including explicit ports)
  untouched while SRV targets surface through `JavaDetails::srv`; status
  payloads with unparseable JSON fail with `Error::InvalidPacket` instead of
  producing a blank status; RakNet and Java SRV probes try every resolved
  address (IPv4 first) within the shared deadline; aggregate errors chain to
  the first probe failure through `Error::source()` and cached-transport
  fallbacks keep both probe failures; `Client::query_with` layers per-query
  options on top of the client defaults instead of replacing them.
- RakNet MOTD decoding is fully allocation-free: a first MOTD line containing
  semicolons is borrowed as one contiguous span instead of being re-joined;
  Java description parsing flattens nested text components (`extra`,
  `translate`).
- Optional concurrency control: `ClientBuilder::merge_in_flight(true)` merges
  concurrent identical queries into one shared probe — every caller receives
  the shared result, including a shared failure, without re-probing, and a late
  joiner shares the currently running probe rather than a fresh window.
  `max_in_flight(n)` bounds the logical queries running at once with one permit
  per query, and `max_queued(n)` bounds the distinct merged probes (and their
  detached tasks) admitted at once. Callers for the same key share one pending
  flight even while the queue is saturated.
- Regression tests for the HTTP cache policy guard, singleflight identity,
  port-aware flight keys, shared failures, caller cancellation, and the
  in-flight limit.

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
- `Error` and `Attempt` implement `Clone`, so a shared singleflight failure can
  be handed to every waiter without re-running the probe. `Error::Io` now holds
  an `Arc<std::io::Error>`, so cloning preserves the original OS error code
  (`raw_os_error()`) and costs one reference count. **Breaking:** the `Io`
  variant's field type changed from `std::io::Error` to `Arc<std::io::Error>`.

## 0.1.0 - Unreleased

- Initial async Minecraft MOTD query library.
- Supports Java Edition status requests and Bedrock Edition RakNet unconnected pings.
- Supports server address normalization, explicit ports, Java SRV lookup, and automatic Java-then-Bedrock probing.
