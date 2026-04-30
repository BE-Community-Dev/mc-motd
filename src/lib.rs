//! Async Minecraft server MOTD queries for Java Edition and Bedrock Edition.
//!
//! The high-level entry points normalize common server address strings, try Java
//! SRV records when appropriate, and then probe Java before Bedrock. Edition
//! specific helpers and packet parsers are also exported for callers that own
//! their transport layer.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use std::time::Duration;
//!
//! use mc_motd::{MinecraftMotd, query_minecraft_address};
//!
//! let result = query_minecraft_address("play.example.com", Duration::from_secs(3)).await?;
//!
//! match result.motd {
//!     MinecraftMotd::Java(status) => println!("{}", status.raw_json),
//!     MinecraftMotd::Bedrock(motd) => println!("{} {}", motd.motd_line_1, motd.version),
//! }
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::time::Duration;

use hickory_resolver::Resolver;
use tokio::time;

pub mod be;
pub mod je;
pub mod utils;

pub use be::{
    BedrockMotd, BedrockMotdRef, build_unconnected_ping_packet, parse_bedrock_motd_text,
    parse_bedrock_pong, query_bedrock, query_bedrock_addr,
};
pub use je::{
    JavaStatus, JavaStatusRef, build_java_status_request, parse_java_status_response, query_java,
    query_java_addr,
};
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
    /// Bedrock Edition unconnected pong response.
    Bedrock,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinecraftMotd {
    /// Java Edition status JSON.
    Java(JavaStatus),
    /// Parsed Bedrock Edition MOTD.
    Bedrock(BedrockMotd),
}

/// Result of an automatic Minecraft MOTD query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinecraftMotdResult {
    /// Edition that responded successfully.
    pub edition: MinecraftEdition,
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
    pub try_srv: bool,
}

impl Default for MotdQueryOptions {
    fn default() -> Self {
        Self {
            java_port: DEFAULT_JAVA_PORT,
            bedrock_port: DEFAULT_BEDROCK_PORT,
            timeout: DEFAULT_TIMEOUT,
            try_srv: true,
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
            | Self::NoCompatibleServer { .. } => None,
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

/// Query one explicit host and port, trying Java Edition before Bedrock Edition.
///
/// SRV lookup is disabled because the caller supplied the port.
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
        },
    )
    .await
}

/// Query a user-facing server address.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`. When no explicit
/// port is present, Java SRV records are tried before direct Java and Bedrock
/// probing with their edition defaults.
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
    let mut attempts = Vec::with_capacity(6);

    if options.try_srv && utils::can_query_srv(&host) {
        match resolve_minecraft_srv(&host, options.timeout).await {
            Ok(targets) if targets.is_empty() => attempts.push("SRV: no records".to_string()),
            Ok(targets) => {
                for target in targets {
                    if let Some(result) = try_java(
                        &target.host,
                        target.port,
                        options.timeout,
                        Some(&target),
                        &mut attempts,
                    )
                    .await
                    {
                        return Ok(result);
                    }
                }
            }
            Err(error) => attempts.push(format!("SRV: {error}")),
        }
    }

    if let Some(result) = try_java(
        &host,
        options.java_port,
        options.timeout,
        None,
        &mut attempts,
    )
    .await
    {
        return Ok(result);
    }
    if let Some(result) = try_bedrock(
        &host,
        options.bedrock_port,
        options.timeout,
        None,
        &mut attempts,
    )
    .await
    {
        return Ok(result);
    }

    Err(MotdError::NoCompatibleServer {
        host,
        java_port: options.java_port,
        bedrock_port: options.bedrock_port,
        attempts,
    })
}

/// Resolve Java Edition `_minecraft._tcp` SRV records for a host.
///
/// IP addresses and IPv6 literals return an empty target list because SRV
/// records only apply to DNS names.
///
/// # Errors
///
/// Returns [`MotdError::InvalidInput`] if the host is invalid,
/// [`MotdError::Timeout`] if the lookup exceeds the timeout, or
/// [`MotdError::Dns`] if resolver setup or lookup fails.
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

async fn try_java(
    host: &str,
    port: u16,
    timeout: Duration,
    srv: Option<&SrvTarget>,
    attempts: &mut Vec<String>,
) -> Option<MinecraftMotdResult> {
    match query_java(host, port, timeout).await {
        Ok(status) => Some(MinecraftMotdResult {
            edition: MinecraftEdition::Java,
            route: srv.map_or(MotdRoute::Direct, |_| MotdRoute::Srv),
            host: host.to_string(),
            port,
            srv: srv.cloned(),
            motd: MinecraftMotd::Java(status),
        }),
        Err(error) => {
            attempts.push(format!("JE {host}:{port}: {error}"));
            None
        }
    }
}

async fn try_bedrock(
    host: &str,
    port: u16,
    timeout: Duration,
    srv: Option<&SrvTarget>,
    attempts: &mut Vec<String>,
) -> Option<MinecraftMotdResult> {
    match query_bedrock(host, port, timeout).await {
        Ok(motd) => Some(MinecraftMotdResult {
            edition: MinecraftEdition::Bedrock,
            route: srv.map_or(MotdRoute::Direct, |_| MotdRoute::Srv),
            host: host.to_string(),
            port,
            srv: srv.cloned(),
            motd: MinecraftMotd::Bedrock(motd),
        }),
        Err(error) => {
            attempts.push(format!("BE {host}:{port}: {error}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };

        assert!(!options.try_srv);
    }
}
