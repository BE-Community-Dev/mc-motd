//! Async Minecraft server MOTD queries for Java Edition and Bedrock Edition.
//!
//! The high-level entry points normalize common server address strings and
//! probe Java and Bedrock **concurrently** (`JavaTcp` on the Java port and, in
//! parallel, both Bedrock transports: the legacy `RakNet` UDP unconnected ping
//! and the `NetherNet` join endpoint `GET https://{host}:{port}/v1/join` used
//! by Bedrock Edition 1.26.51+). The first compatible response wins, with Java
//! preferred on a tie; inside the Bedrock race the faster transport wins, with
//! `NetherNet` preferred on ties. The transport that answers is cached per
//! `host:port` so later queries skip the failing probe.
//!
//! Editions are isolatable: the `java` cargo feature (on by default) controls
//! whether Java probing, SRV lookup, and the `hickory-resolver` dependency are
//! compiled at all, and [`MotdQueryOptions::try_java`] or the Bedrock-only
//! [`query_bedrock_auto`]/[`query_bedrock_address`] entry points skip Java per
//! call. For hot polling paths, [`query_bedrock_addr_into`] parses the pong
//! into a caller-owned buffer and returns a borrowed [`BedrockMotdRef`], and
//! [`parse_nethernet_join_json_ref`] borrows `NetherNet` JSON fields without
//! allocating.
//!
//! The reported latency is the round trip of the query itself. For
//! [`Transport::NetherNetHttp`] this is HTTPS/HTTP signaling latency and is
//! *not* the game connection RTT; the game runs over WebRTC/UDP after the join
//! handshake.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use std::time::Duration;
//!
//! use mc_motd::{MinecraftMotd, query_minecraft_address};
//!
//! let result = query_minecraft_address("play.example.com", Duration::from_secs(3)).await?;
//!
//! println!("transport={:?} latency={:?}", result.transport, result.latency);
//! match result.motd {
//!     MinecraftMotd::Java(status) => println!("{}", status.raw_json),
//!     MinecraftMotd::Bedrock(motd) => println!("{} {}", motd.motd_line_1, motd.version),
//!     MinecraftMotd::BedrockNetherNet(motd) => println!("{} {}", motd.name, motd.version),
//!     // The enum is non_exhaustive so future editions can be added without
//!     // another breaking change.
//!     _ => {}
//! }
//! # Ok(())
//! # }
//! ```

use std::fmt;
#[cfg(any(feature = "java", feature = "nethernet"))]
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

#[cfg(feature = "java")]
use hickory_resolver::Resolver;
use tokio::net::lookup_host;
use tokio::time;

pub mod be;
#[cfg(feature = "java")]
pub mod je;
pub mod lan;
#[cfg(feature = "nethernet")]
pub mod transport_cache;
pub mod utils;

#[cfg(feature = "nethernet")]
pub use be::nethernet::{
    BedrockNetherNetMotd, BedrockNetherNetMotdRef, NetherNetQueryOptions,
    parse_nethernet_join_json, parse_nethernet_join_json_ref, query_nethernet, query_nethernet_url,
    shared_client,
};
pub use be::{
    BedrockMotd, BedrockMotdRef, build_unconnected_ping_packet, parse_bedrock_motd_text,
    parse_bedrock_pong, query_bedrock, query_bedrock_addr, query_bedrock_addr_into,
};
#[cfg(feature = "java")]
pub use je::{
    JavaStatus, JavaStatusRef, build_java_status_request, parse_java_status_response, query_java,
    query_java_addr,
};
pub use lan::{
    DEFAULT_LAN_PORT, LanDiscoveryOptions, LanServer, VANILLA_LAN_PORT, discover_lan_servers,
};
#[cfg(feature = "nethernet")]
pub use transport_cache::{cached_transport, clear_transport_cache};
pub use utils::{normalize_server_host, parse_server_address, validate_port, validate_server_host};

/// Default Java Edition server port.
pub const DEFAULT_JAVA_PORT: u16 = 25_565;
/// Default Bedrock Edition server port.
pub const DEFAULT_BEDROCK_PORT: u16 = 19_132;
/// Default query timeout used by convenience options.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Minecraft server edition detected by a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinecraftEdition {
    /// Java Edition status response.
    Java,
    /// Bedrock Edition response, either `RakNet` unconnected pong or `NetherNet`
    /// join response.
    Bedrock,
}

/// Transport that produced a query response.
///
/// The transport identifies how the MOTD was fetched. For Bedrock servers this
/// distinguishes the legacy `RakNet` ping from the `NetherNet` join endpoint; the
/// two latencies have different meanings and must not be equated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Java Edition status query over TCP.
    JavaTcp,
    /// Bedrock Edition `RakNet` unconnected ping over UDP.
    RakNetUdp,
    /// Bedrock Edition `NetherNet` join endpoint over HTTPS/HTTP.
    NetherNetHttp,
}

impl fmt::Display for Transport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JavaTcp => formatter.write_str("Java TCP"),
            Self::RakNetUdp => formatter.write_str("RakNet UDP"),
            Self::NetherNetHttp => formatter.write_str("NetherNet HTTP"),
        }
    }
}

/// Route used to reach a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotdRoute {
    /// Queried the supplied host and port directly.
    Direct,
    /// Queried a target resolved from a Java Edition SRV record.
    Srv,
}

/// Normalized server address and edition-specific ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    /// Normalized hostname or IP address.
    pub host: String,
    /// Java Edition port to query.
    pub java_port: u16,
    /// Bedrock Edition port to query.
    pub bedrock_port: u16,
    /// Explicit port from user input, if one was present.
    pub explicit_port: Option<u16>,
}

/// Java Edition SRV lookup target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvTarget {
    /// Target hostname with the trailing DNS root dot removed.
    pub host: String,
    /// Target port from the SRV record.
    pub port: u16,
    /// SRV priority. Lower values are preferred.
    pub priority: u16,
    /// SRV weight. Higher values are preferred within the same priority.
    pub weight: u16,
}

/// Edition-specific MOTD payload.
///
/// The enum is `non_exhaustive` so new editions or transports can be added
/// without another breaking release; match arms should include a wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MinecraftMotd {
    /// Java Edition status JSON.
    #[cfg(feature = "java")]
    Java(JavaStatus),
    /// Parsed Bedrock Edition MOTD from a `RakNet` unconnected pong.
    Bedrock(BedrockMotd),
    /// Parsed Bedrock Edition MOTD from a `NetherNet` join response.
    #[cfg(feature = "nethernet")]
    BedrockNetherNet(BedrockNetherNetMotd),
}

/// Result of an automatic Minecraft MOTD query.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MinecraftMotdResult {
    /// Edition that responded successfully.
    pub edition: MinecraftEdition,
    /// Transport that produced the response.
    pub transport: Transport,
    /// Round-trip time of the successful query.
    ///
    /// For [`Transport::JavaTcp`] and [`Transport::RakNetUdp`] this approximates
    /// the game connection path. For [`Transport::NetherNetHttp`] this is HTTPS
    /// signaling latency only; it is not the game connection RTT.
    pub latency: Duration,
    /// Route used to reach the server.
    pub route: MotdRoute,
    /// Host that was queried.
    pub host: String,
    /// Port that was queried.
    pub port: u16,
    /// SRV record used for the query, if any.
    pub srv: Option<SrvTarget>,
    /// Edition-specific response payload.
    pub motd: MinecraftMotd,
}

/// Options for automatic Java/Bedrock probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotdQueryOptions {
    /// Java Edition port used for direct probing.
    pub java_port: u16,
    /// Bedrock Edition port used for direct probing.
    pub bedrock_port: u16,
    /// Timeout applied to DNS lookup and network I/O.
    pub timeout: Duration,
    /// Whether Java SRV lookup is attempted before direct probing.
    ///
    /// SRV lookup is a Java Edition concept, so setting [`MotdQueryOptions::try_java`]
    /// to `false` also skips it.
    pub try_srv: bool,
    /// Whether Java Edition probing (and SRV lookup) runs at all.
    ///
    /// Bedrock-focused callers can set this to `false` to probe only the
    /// Bedrock transports, or use [`query_bedrock_auto`]. This also has no
    /// effect when the crate is built without the `java` cargo feature, which
    /// removes Java support entirely.
    pub try_java: bool,
}

impl Default for MotdQueryOptions {
    fn default() -> Self {
        Self {
            java_port: DEFAULT_JAVA_PORT,
            bedrock_port: DEFAULT_BEDROCK_PORT,
            timeout: DEFAULT_TIMEOUT,
            try_srv: true,
            try_java: true,
        }
    }
}

impl MotdQueryOptions {
    /// Build options that use the same explicit port for both editions.
    #[must_use]
    pub fn with_port(port: u16, timeout: Duration) -> Self {
        Self {
            java_port: port,
            bedrock_port: port,
            timeout,
            try_srv: true,
            try_java: true,
        }
    }
}

/// Error returned by MOTD parsing and query operations.
#[derive(Debug)]
pub enum MotdError {
    /// DNS resolution returned no usable socket address.
    ResolveFailed,
    /// Network or socket I/O failed.
    Io(std::io::Error),
    /// A DNS lookup or network operation exceeded the configured timeout.
    Timeout,
    /// DNS resolver setup or lookup failed.
    Dns(String),
    /// Caller supplied an invalid host, address, or port.
    InvalidInput(&'static str),
    /// The remote server returned an invalid or unsupported packet.
    InvalidPacket(&'static str),
    /// A response payload was not valid UTF-8.
    InvalidUtf8(std::str::Utf8Error),
    /// An HTTP endpoint answered with an unexpected status code.
    HttpStatus(u16),
    /// An HTTP request failed before or during the exchange.
    Http(String),
    /// Automatic Java and Bedrock probing both failed.
    NoCompatibleServer {
        /// Original normalized host.
        host: String,
        /// Java Edition port that was attempted.
        java_port: u16,
        /// Bedrock Edition port that was attempted.
        bedrock_port: u16,
        /// Human-readable attempt failures.
        attempts: Vec<String>,
    },
    /// Bedrock-only probing failed; Java was skipped by the caller or was not
    /// compiled in.
    NoCompatibleBedrockServer {
        /// Original normalized host.
        host: String,
        /// Bedrock Edition port that was attempted.
        bedrock_port: u16,
        /// Human-readable attempt failures.
        attempts: Vec<String>,
    },
}

impl fmt::Display for MotdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResolveFailed => formatter.write_str("server address could not be resolved"),
            Self::Io(error) => write!(formatter, "network error: {error}"),
            Self::Timeout => formatter.write_str("server query timed out"),
            Self::Dns(error) => write!(formatter, "DNS lookup failed: {error}"),
            Self::InvalidInput(message) => write!(formatter, "invalid server address: {message}"),
            Self::InvalidPacket(message) => {
                write!(formatter, "invalid Minecraft response: {message}")
            }
            Self::InvalidUtf8(error) => write!(formatter, "invalid UTF-8 MOTD: {error}"),
            Self::HttpStatus(status) => {
                write!(formatter, "server returned HTTP status {status}")
            }
            Self::Http(error) => write!(formatter, "HTTP request failed: {error}"),
            Self::NoCompatibleServer {
                host,
                java_port,
                bedrock_port,
                attempts,
            } => {
                write!(
                    formatter,
                    "no compatible Minecraft server found at {host} (JE:{java_port}, BE:{bedrock_port})"
                )?;
                if !attempts.is_empty() {
                    write!(formatter, ": {}", attempts.join("; "))?;
                }
                Ok(())
            }
            Self::NoCompatibleBedrockServer {
                host,
                bedrock_port,
                attempts,
            } => {
                write!(
                    formatter,
                    "no compatible Bedrock server found at {host} (BE:{bedrock_port})"
                )?;
                if !attempts.is_empty() {
                    write!(formatter, ": {}", attempts.join("; "))?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for MotdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            Self::ResolveFailed
            | Self::Timeout
            | Self::Dns(_)
            | Self::InvalidInput(_)
            | Self::InvalidPacket(_)
            | Self::HttpStatus(_)
            | Self::Http(_)
            | Self::NoCompatibleServer { .. }
            | Self::NoCompatibleBedrockServer { .. } => None,
        }
    }
}

impl From<std::io::Error> for MotdError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<std::str::Utf8Error> for MotdError {
    fn from(error: std::str::Utf8Error) -> Self {
        Self::InvalidUtf8(error)
    }
}

/// Query one explicit host and port across Java and Bedrock concurrently.
///
/// Java (TCP status) and Bedrock (`RakNet` plus `NetherNet`) probes run in
/// parallel on the same port; the first compatible response wins, with Java
/// preferred on a tie. SRV lookup is disabled because the caller supplied the
/// port.
///
/// # Errors
///
/// Returns [`MotdError`] if the host or port is invalid, DNS resolution fails,
/// network I/O times out or fails, both editions reject the response, or no
/// compatible server answers.
pub async fn query_minecraft(
    host: impl AsRef<str>,
    port: u16,
    timeout: Duration,
) -> Result<MinecraftMotdResult, MotdError> {
    validate_port(port)?;
    let host = normalize_server_host(host.as_ref())?.into_owned();
    query_minecraft_ordered(
        host,
        MotdQueryOptions {
            java_port: port,
            bedrock_port: port,
            timeout,
            try_srv: false,
            try_java: true,
        },
    )
    .await
}

/// Query a user-facing server address.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`. When no explicit
/// port is present, Java SRV records are resolved first, then Java and Bedrock
/// probes run concurrently with their edition defaults.
///
/// # Errors
///
/// Returns [`MotdError`] if the address is invalid, DNS lookup fails, network
/// I/O times out or fails, packets cannot be parsed, or no compatible server is
/// found.
pub async fn query_minecraft_address(
    address: impl AsRef<str>,
    timeout: Duration,
) -> Result<MinecraftMotdResult, MotdError> {
    let address = parse_server_address(address.as_ref(), DEFAULT_JAVA_PORT, DEFAULT_BEDROCK_PORT)?;
    query_minecraft_with_options(
        &address.host,
        MotdQueryOptions {
            java_port: address.java_port,
            bedrock_port: address.bedrock_port,
            timeout,
            try_srv: address.explicit_port.is_none(),
            try_java: true,
        },
    )
    .await
}

/// Query a Bedrock Edition server by host and port across both transports.
///
/// This is the Bedrock-only entry point: Java Edition is never probed, SRV
/// records are never consulted, and neither the `java` cargo feature nor
/// [`MotdQueryOptions::try_java`] matters here. The `RakNet` and `NetherNet`
/// probes race concurrently — the faster transport wins and `NetherNet` is
/// preferred on ties — and the transport that answers is cached for later
/// queries.
///
/// # Errors
///
/// Returns [`MotdError::NoCompatibleBedrockServer`] when neither Bedrock
/// transport answers, and [`MotdError`] for invalid input, DNS, and I/O
/// failures.
pub async fn query_bedrock_auto(
    host: impl AsRef<str>,
    port: u16,
    timeout: Duration,
) -> Result<MinecraftMotdResult, MotdError> {
    validate_port(port)?;
    let host = normalize_server_host(host.as_ref())?.into_owned();
    bedrock_only_query(
        host,
        MotdQueryOptions {
            java_port: port,
            bedrock_port: port,
            timeout,
            try_srv: false,
            try_java: false,
        },
    )
    .await
}

/// Query a user-facing Bedrock Edition server address across both transports.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`; the default
/// Bedrock port is used when no explicit port is present. Java Edition is
/// never probed.
///
/// # Errors
///
/// Returns [`MotdError::NoCompatibleBedrockServer`] when neither Bedrock
/// transport answers, and [`MotdError`] for invalid input, DNS, and I/O
/// failures.
pub async fn query_bedrock_address(
    address: impl AsRef<str>,
    timeout: Duration,
) -> Result<MinecraftMotdResult, MotdError> {
    let address = parse_server_address(address.as_ref(), DEFAULT_JAVA_PORT, DEFAULT_BEDROCK_PORT)?;
    bedrock_only_query(
        address.host,
        MotdQueryOptions {
            java_port: address.java_port,
            bedrock_port: address.bedrock_port,
            timeout,
            try_srv: false,
            try_java: false,
        },
    )
    .await
}

/// Query a host with explicit probing options.
///
/// # Errors
///
/// Returns [`MotdError`] if ports or host input are invalid, SRV lookup fails
/// and direct probing also fails, network operations time out or fail, or all
/// attempted responses are invalid.
pub async fn query_minecraft_with_options(
    host: impl AsRef<str>,
    options: MotdQueryOptions,
) -> Result<MinecraftMotdResult, MotdError> {
    validate_port(options.java_port)?;
    validate_port(options.bedrock_port)?;

    let host = normalize_server_host(host.as_ref())?.into_owned();
    query_minecraft_ordered(host, options).await
}

async fn query_minecraft_ordered(
    host: String,
    options: MotdQueryOptions,
) -> Result<MinecraftMotdResult, MotdError> {
    #[cfg(feature = "java")]
    if options.try_java {
        return query_java_then_bedrock(host, options).await;
    }
    bedrock_only_query(host, options).await
}

/// Java-plus-Bedrock probing: SRV records first, then a concurrent race where
/// Java is preferred and Bedrock runs in parallel.
#[cfg(feature = "java")]
async fn query_java_then_bedrock(
    host: String,
    options: MotdQueryOptions,
) -> Result<MinecraftMotdResult, MotdError> {
    let mut prefix_attempts = Vec::new();

    if options.try_srv && utils::can_query_srv(&host) {
        match resolve_minecraft_srv(&host, options.timeout).await {
            Ok(targets) if !targets.is_empty() => {
                // SRV records are authoritative for the address, so the Java
                // probe (SRV targets first, then direct) stays preferred even
                // though the Bedrock probe runs concurrently.
                let java_future = probe_java_with_srv(&host, &options, &targets);
                let bedrock_future =
                    bedrock_probe(&host, options.bedrock_port, options.timeout, None);
                return race_probe(java_future, bedrock_future)
                    .await
                    .map_err(|attempts| no_compatible_server(host, &options, attempts));
            }
            Ok(_) => prefix_attempts.push("SRV: no records".to_string()),
            Err(error) => prefix_attempts.push(format!("SRV: {error}")),
        }
    }

    // Resolve the host once and share the address between the Java and
    // Bedrock direct probes instead of looking it up twice.
    let resolved = match resolve_probe_ip(&host, options.timeout).await {
        Ok(ip) => ip,
        Err(error) => {
            prefix_attempts.push(format!("DNS {host}: {error}"));
            return Err(no_compatible_server(host, &options, prefix_attempts));
        }
    };

    let java_future = java_direct_probe(
        &host,
        options.java_port,
        options.timeout,
        Some(SocketAddr::new(resolved, options.java_port)),
    );
    let bedrock_future = bedrock_probe(
        &host,
        options.bedrock_port,
        options.timeout,
        Some(SocketAddr::new(resolved, options.bedrock_port)),
    );
    race_probe(java_future, bedrock_future)
        .await
        .map_err(|attempts| {
            prefix_attempts.extend(attempts);
            no_compatible_server(host, &options, prefix_attempts)
        })
}

/// Bedrock-only probing used by [`query_bedrock_auto`], [`query_bedrock_address`],
/// and automatic queries with [`MotdQueryOptions::try_java`] disabled.
async fn bedrock_only_query(
    host: String,
    options: MotdQueryOptions,
) -> Result<MinecraftMotdResult, MotdError> {
    let bedrock_port = options.bedrock_port;
    let resolved = match resolve_probe_ip(&host, options.timeout).await {
        Ok(ip) => SocketAddr::new(ip, bedrock_port),
        Err(error) => {
            let attempts = vec![format!("DNS {host}: {error}")];
            return Err(no_compatible_bedrock(host, bedrock_port, attempts));
        }
    };
    bedrock_probe(&host, bedrock_port, options.timeout, Some(resolved))
        .await
        .map_err(|attempts| no_compatible_bedrock(host, bedrock_port, attempts))
}

/// Probe SRV targets in priority order, then fall back to a direct probe.
#[cfg(feature = "java")]
async fn probe_java_with_srv(
    host: &str,
    options: &MotdQueryOptions,
    targets: &[SrvTarget],
) -> Result<MinecraftMotdResult, Vec<String>> {
    let mut attempts = Vec::new();
    for target in targets {
        match try_java(&target.host, target.port, options.timeout, Some(target)).await {
            Ok(result) => return Ok(result),
            Err(error) => attempts.push(error),
        }
    }
    match try_java(host, options.java_port, options.timeout, None).await {
        Ok(result) => Ok(result),
        Err(error) => {
            attempts.push(error);
            Err(attempts)
        }
    }
}

/// Probe the Java Edition direct port at a pre-resolved or lazily resolved
/// address.
#[cfg(feature = "java")]
async fn java_direct_probe(
    host: &str,
    port: u16,
    timeout: Duration,
    resolved: Option<SocketAddr>,
) -> Result<MinecraftMotdResult, Vec<String>> {
    match resolved {
        Some(address) => try_java_addr(host, port, address, timeout)
            .await
            .map_err(|error| vec![error]),
        None => try_java(host, port, timeout, None)
            .await
            .map_err(|error| vec![error]),
    }
}

/// Race two probes that run concurrently, preferring the first future.
///
/// Both futures are polled from the start, so the fallback probe is already
/// running while the preferred probe is awaited. When the preferred probe
/// fails, the fallback result is usually ready immediately. Attempt errors are
/// only collected when both probes fail, keeping the success path
/// allocation-free.
#[cfg(any(feature = "java", feature = "nethernet"))]
async fn race_probe<P, F>(preferred: P, fallback: F) -> Result<MinecraftMotdResult, Vec<String>>
where
    P: Future<Output = Result<MinecraftMotdResult, Vec<String>>>,
    F: Future<Output = Result<MinecraftMotdResult, Vec<String>>>,
{
    tokio::pin!(preferred);
    tokio::pin!(fallback);

    let mut attempts = Vec::new();
    let mut preferred_alive = true;
    let mut fallback_alive = true;
    loop {
        tokio::select! {
            biased;
            result = &mut preferred, if preferred_alive => {
                match result {
                    Ok(found) => return Ok(found),
                    Err(messages) => {
                        attempts.extend(messages);
                        preferred_alive = false;
                    }
                }
            }
            result = &mut fallback, if fallback_alive => {
                match result {
                    Ok(found) => return Ok(found),
                    Err(messages) => {
                        attempts.extend(messages);
                        fallback_alive = false;
                    }
                }
            }
            else => return Err(attempts),
        }
    }
}

/// Resolve a host once and return the first address for probe port mapping.
async fn resolve_probe_ip(host: &str, timeout: Duration) -> Result<IpAddr, MotdError> {
    let mut resolved = time::timeout(timeout, lookup_host((host, 0)))
        .await
        .map_err(|_| MotdError::Timeout)??;
    let address = resolved.next().ok_or(MotdError::ResolveFailed)?;
    Ok(address.ip())
}

#[cfg(feature = "java")]
fn no_compatible_server(
    host: String,
    options: &MotdQueryOptions,
    attempts: Vec<String>,
) -> MotdError {
    MotdError::NoCompatibleServer {
        host,
        java_port: options.java_port,
        bedrock_port: options.bedrock_port,
        attempts,
    }
}

fn no_compatible_bedrock(host: String, bedrock_port: u16, attempts: Vec<String>) -> MotdError {
    MotdError::NoCompatibleBedrockServer {
        host,
        bedrock_port,
        attempts,
    }
}

/// Resolve Java Edition `_minecraft._tcp` SRV records for a host.
///
/// IP addresses and IPv6 literals return an empty target list because SRV
/// records only apply to DNS names. Only available with the `java` cargo
/// feature.
///
/// # Errors
///
/// Returns [`MotdError::InvalidInput`] if the host is invalid,
/// [`MotdError::Timeout`] if the lookup exceeds the timeout, or
/// [`MotdError::Dns`] if resolver setup or lookup fails.
#[cfg(feature = "java")]
pub async fn resolve_minecraft_srv(
    host: impl AsRef<str>,
    timeout: Duration,
) -> Result<Vec<SrvTarget>, MotdError> {
    let host = normalize_server_host(host.as_ref())?;
    if !utils::can_query_srv(host.as_ref()) {
        return Ok(Vec::new());
    }

    let resolver = Resolver::builder_tokio()
        .map_err(|error| MotdError::Dns(error.to_string()))?
        .build();
    let name = utils::minecraft_srv_name(host.as_ref());
    let lookup = time::timeout(timeout, resolver.srv_lookup(name))
        .await
        .map_err(|_| MotdError::Timeout)?
        .map_err(|error| MotdError::Dns(error.to_string()))?;

    let mut targets = lookup
        .iter()
        .map(|srv| SrvTarget {
            host: utils::normalize_srv_target(&srv.target().to_utf8()),
            port: srv.port(),
            priority: srv.priority(),
            weight: srv.weight(),
        })
        .filter(|target| !target.host.is_empty() && target.port != 0)
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| right.weight.cmp(&left.weight))
            .then_with(|| left.host.cmp(&right.host))
            .then_with(|| left.port.cmp(&right.port))
    });

    Ok(targets)
}

/// Probe a Java Edition server by host and port, returning the failure message
/// on error so attempt lists are only built when probes fail.
#[cfg(feature = "java")]
async fn try_java(
    host: &str,
    port: u16,
    timeout: Duration,
    srv: Option<&SrvTarget>,
) -> Result<MinecraftMotdResult, String> {
    let started = Instant::now();
    match query_java(host, port, timeout).await {
        Ok(status) => {
            let latency = started.elapsed();
            Ok(MinecraftMotdResult {
                edition: MinecraftEdition::Java,
                transport: Transport::JavaTcp,
                latency,
                route: srv.map_or(MotdRoute::Direct, |_| MotdRoute::Srv),
                host: host.to_string(),
                port,
                srv: srv.cloned(),
                motd: MinecraftMotd::Java(status),
            })
        }
        Err(error) => Err(format!("JE {host}:{port}: {error}")),
    }
}

/// Probe a Java Edition server at a pre-resolved address; the host and port
/// are still written into the handshake.
#[cfg(feature = "java")]
async fn try_java_addr(
    host: &str,
    port: u16,
    address: SocketAddr,
    timeout: Duration,
) -> Result<MinecraftMotdResult, String> {
    let started = Instant::now();
    match query_java_addr(address, host, port, -1, timeout).await {
        Ok(status) => {
            let latency = started.elapsed();
            Ok(MinecraftMotdResult {
                edition: MinecraftEdition::Java,
                transport: Transport::JavaTcp,
                latency,
                route: MotdRoute::Direct,
                host: host.to_string(),
                port,
                srv: None,
                motd: MinecraftMotd::Java(status),
            })
        }
        Err(error) => Err(format!("JE {host}:{port}: {error}")),
    }
}

/// Probe a Bedrock server, honoring the cached transport when one is known.
///
/// Without the `nethernet` feature this is a plain `RakNet` probe.
async fn bedrock_probe(
    host: &str,
    port: u16,
    timeout: Duration,
    resolved: Option<SocketAddr>,
) -> Result<MinecraftMotdResult, Vec<String>> {
    #[cfg(not(feature = "nethernet"))]
    return try_raknet(host, port, timeout, resolved).await;

    #[cfg(feature = "nethernet")]
    bedrock_probe_with_nethernet(host, port, timeout, resolved).await
}

/// Probe a Bedrock server across both transports with transport caching.
///
/// The cached transport is attempted first. When no transport is cached yet,
/// both transports are raced concurrently: the faster transport wins and
/// `NetherNet` is preferred on ties, so third-party servers that answer both
/// protocols always get the quickest available answer.
#[cfg(feature = "nethernet")]
async fn bedrock_probe_with_nethernet(
    host: &str,
    port: u16,
    timeout: Duration,
    resolved: Option<SocketAddr>,
) -> Result<MinecraftMotdResult, Vec<String>> {
    match transport_cache::cached_transport(host, port) {
        Some(Transport::NetherNetHttp) => {
            if let Ok(result) = try_nethernet(host, port, timeout).await {
                return Ok(result);
            }
            try_raknet(host, port, timeout, resolved).await
        }
        Some(Transport::RakNetUdp) => {
            if let Ok(result) = try_raknet(host, port, timeout, resolved).await {
                return Ok(result);
            }
            try_nethernet(host, port, timeout).await
        }
        _ => {
            let nether_future = try_nethernet(host, port, timeout);
            let raknet_future = try_raknet(host, port, timeout, resolved);
            race_probe(nether_future, raknet_future).await
        }
    }
}

/// Probe the `NetherNet` join endpoint, caching the transport on success.
#[cfg(feature = "nethernet")]
async fn try_nethernet(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<MinecraftMotdResult, Vec<String>> {
    match query_nethernet(host, port, timeout).await {
        Ok(motd) => {
            transport_cache::store_transport(host, port, Transport::NetherNetHttp);
            let latency = motd.latency;
            Ok(MinecraftMotdResult {
                edition: MinecraftEdition::Bedrock,
                transport: Transport::NetherNetHttp,
                latency,
                route: MotdRoute::Direct,
                host: host.to_string(),
                port,
                srv: None,
                motd: MinecraftMotd::BedrockNetherNet(motd),
            })
        }
        Err(error) => Err(vec![format!("BE NetherNet {host}:{port}: {error}")]),
    }
}

/// Probe the `RakNet` UDP ping at a pre-resolved or lazily resolved address,
/// caching the transport on success.
async fn try_raknet(
    host: &str,
    port: u16,
    timeout: Duration,
    resolved: Option<SocketAddr>,
) -> Result<MinecraftMotdResult, Vec<String>> {
    let started = Instant::now();
    let query = async {
        match resolved {
            Some(address) => query_bedrock_addr(address, timeout).await,
            None => query_bedrock(host, port, timeout).await,
        }
    };
    match query.await {
        Ok(motd) => {
            let latency = started.elapsed();
            #[cfg(feature = "nethernet")]
            transport_cache::store_transport(host, port, Transport::RakNetUdp);
            Ok(MinecraftMotdResult {
                edition: MinecraftEdition::Bedrock,
                transport: Transport::RakNetUdp,
                latency,
                route: MotdRoute::Direct,
                host: host.to_string(),
                port,
                srv: None,
                motd: MinecraftMotd::Bedrock(motd),
            })
        }
        Err(error) => Err(vec![format!("BE RakNet {host}:{port}: {error}")]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOCK_MOTD_TEXT: &str =
        "MCPE;RakNet Mock;589;1.21.100;3;20;12345;Line2;Survival;1;19132;19133;";

    #[test]
    fn default_options_use_edition_ports() {
        let options = MotdQueryOptions::default();

        assert_eq!(options.java_port, DEFAULT_JAVA_PORT);
        assert_eq!(options.bedrock_port, DEFAULT_BEDROCK_PORT);
        assert!(options.try_srv);
    }

    #[test]
    fn shared_port_options_use_one_port_for_both_editions() {
        let options = MotdQueryOptions::with_port(19_132, DEFAULT_TIMEOUT);

        assert_eq!(options.java_port, 19_132);
        assert_eq!(options.bedrock_port, 19_132);
    }

    #[test]
    fn explicit_port_query_disables_srv() {
        let options = MotdQueryOptions {
            java_port: 19_132,
            bedrock_port: 19_132,
            timeout: DEFAULT_TIMEOUT,
            try_srv: false,
            try_java: true,
        };

        assert!(!options.try_srv);
        assert!(options.try_java);
    }

    #[test]
    fn transport_display_names_transports() {
        assert_eq!(Transport::JavaTcp.to_string(), "Java TCP");
        assert_eq!(Transport::RakNetUdp.to_string(), "RakNet UDP");
        assert_eq!(Transport::NetherNetHttp.to_string(), "NetherNet HTTP");
    }

    /// Build a `RakNet` unconnected pong packet carrying the MOTD text.
    fn build_mock_pong(text: &str) -> Vec<u8> {
        let mut packet = Vec::with_capacity(35 + text.len());
        packet.push(0x1c);
        packet.extend_from_slice(&1_i64.to_be_bytes());
        packet.extend_from_slice(&2_i64.to_be_bytes());
        packet.extend_from_slice(&[
            0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34,
            0x56, 0x78,
        ]);
        packet.extend_from_slice(
            &u16::try_from(text.len())
                .expect("fixture MOTD length fits in u16")
                .to_be_bytes(),
        );
        packet.extend_from_slice(text.as_bytes());
        packet
    }

    /// Spawn a UDP socket that answers every unconnected ping with a pong.
    async fn spawn_raknet_pong_mock() -> std::net::SocketAddr {
        use std::net::Ipv4Addr;
        use tokio::net::UdpSocket;

        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock socket should bind");
        let address = socket.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let mut buffer = [0_u8; 64];
            loop {
                let Ok((len, peer)) = socket.recv_from(&mut buffer).await else {
                    return;
                };
                if len != 33 {
                    return;
                }
                let pong = build_mock_pong(MOCK_MOTD_TEXT);
                let _ = socket.send_to(&pong, peer).await;
            }
        });
        address
    }

    #[cfg(feature = "java")]
    mod java_race_tests {
        use super::*;

        use std::net::Ipv4Addr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        const JAVA_MOCK_JSON: &str =
            r#"{"version":{"name":"1.21.4"},"players":{"online":1,"max":20}}"#;

        /// Spawn a mock Java status server that answers every connection.
        async fn spawn_java_status_mock() -> std::net::SocketAddr {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("java mock should bind");
            let address = listener
                .local_addr()
                .expect("java mock address should exist");
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let mut request = [0_u8; 256];
                    let _ = stream.read(&mut request).await;
                    let _ = stream
                        .write_all(&build_java_status_response(JAVA_MOCK_JSON))
                        .await;
                }
            });
            address
        }

        /// Build a Java status response frame with single-byte varints.
        fn build_java_status_response(json: &str) -> Vec<u8> {
            let mut packet = vec![0x00];
            packet.push(
                u8::try_from(json.len()).expect("fixture JSON length fits in one varint byte"),
            );
            packet.extend_from_slice(json.as_bytes());
            let mut frame = vec![
                u8::try_from(packet.len()).expect("fixture packet length fits in one varint byte"),
            ];
            frame.extend_from_slice(&packet);
            frame
        }

        #[tokio::test]
        async fn race_returns_java_response() {
            let java_address = spawn_java_status_mock().await;
            // No RakNet responder exists, so only the Java probe can answer.
            let result = query_minecraft("127.0.0.1", java_address.port(), Duration::from_secs(3))
                .await
                .expect("java mock should win the race");
            assert_eq!(result.edition, MinecraftEdition::Java);
            assert_eq!(result.transport, Transport::JavaTcp);
            assert!(result.latency > Duration::ZERO);
        }
    }

    mod bedrock_auto_tests {
        use super::*;

        #[tokio::test]
        async fn bedrock_auto_answers_via_raknet() {
            let address = spawn_raknet_pong_mock().await;
            let host = address.ip().to_string();
            let port = address.port();

            let result = query_bedrock_auto(&host, port, Duration::from_secs(3))
                .await
                .expect("raknet mock should answer the bedrock-only query");

            assert_eq!(result.edition, MinecraftEdition::Bedrock);
            assert_eq!(result.transport, Transport::RakNetUdp);
            match &result.motd {
                MinecraftMotd::Bedrock(motd) => assert_eq!(motd.motd_line_1, "RakNet Mock"),
                _ => panic!("expected a RakNet motd payload"),
            }
        }

        #[tokio::test]
        async fn bedrock_auto_reports_bedrock_only_error() {
            // Port 1 on the loopback interface is virtually always closed, so
            // both Bedrock transports fail.
            let error = query_bedrock_auto("127.0.0.1", 1, Duration::from_millis(300))
                .await
                .expect_err("closed port should fail");

            assert!(matches!(
                error,
                MotdError::NoCompatibleBedrockServer {
                    bedrock_port: 1,
                    ..
                }
            ));
        }
    }

    #[cfg(feature = "nethernet")]
    mod nethernet_tests {
        use super::*;

        #[tokio::test]
        async fn auto_probe_falls_back_to_raknet_and_caches_transport() {
            // Hold the shared cache lock for the whole test so no other test's
            // cache clears can race with the query's cache writes.
            let _guard = crate::transport_cache::TEST_LOCK.lock().await;
            let address = spawn_raknet_pong_mock().await;
            let host = address.ip().to_string();
            let port = address.port();

            let result = query_minecraft(&host, port, Duration::from_secs(3))
                .await
                .expect("raknet fallback should answer");
            assert_eq!(result.edition, MinecraftEdition::Bedrock);
            assert_eq!(result.transport, Transport::RakNetUdp);
            assert_eq!(result.port, port);
            assert_eq!(
                cached_transport(&host, port),
                Some(Transport::RakNetUdp),
                "successful raknet transport should be cached"
            );

            let second = query_minecraft(&host, port, Duration::from_secs(3))
                .await
                .expect("cached transport query should answer");
            assert_eq!(second.transport, Transport::RakNetUdp);

            clear_transport_cache();
        }

        #[tokio::test]
        #[ignore = "queries public Minecraft servers; run with --ignored"]
        async fn live_auto_mode_answers_via_bedrock_race() {
            let _guard = crate::transport_cache::TEST_LOCK.lock().await;
            let host = "nethernet.playhive.com";
            let result = query_minecraft(host, 19_132, Duration::from_secs(5))
                .await
                .expect("hive should answer");
            assert_eq!(result.edition, MinecraftEdition::Bedrock);
            // The Bedrock race answers with whichever transport responds first
            // (RakNet is usually faster; NetherNet is preferred on ties).
            assert_ne!(result.transport, Transport::JavaTcp);
            assert!(result.latency > Duration::ZERO);

            let cached = query_minecraft(host, 19_132, Duration::from_secs(5))
                .await
                .expect("second query should answer");
            assert_eq!(cached.transport, result.transport);

            clear_transport_cache();
        }
    }
}
