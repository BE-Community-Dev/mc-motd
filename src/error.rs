//! Structured error model for query and parsing failures.

use std::fmt;

use crate::status::Transport;

/// Result alias used by every fallible API in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Error returned by query and parsing operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// DNS resolution returned no usable socket address.
    ResolveFailed,
    /// Network or socket I/O failed.
    Io(std::io::Error),
    /// A DNS lookup or network operation exceeded the configured deadline.
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
        /// One structured entry per failed probe.
        attempts: Vec<Attempt>,
    },
    /// Java-only probing failed; Bedrock was skipped by the caller or was not
    /// compiled in.
    NoCompatibleJavaServer {
        /// Original normalized host.
        host: String,
        /// Java Edition port that was attempted.
        java_port: u16,
        /// One structured entry per failed probe.
        attempts: Vec<Attempt>,
    },
    /// Bedrock-only probing failed; Java was skipped by the caller or was not
    /// compiled in.
    NoCompatibleBedrockServer {
        /// Original normalized host.
        host: String,
        /// Bedrock Edition port that was attempted.
        bedrock_port: u16,
        /// One structured entry per failed probe.
        attempts: Vec<Attempt>,
    },
}

/// One failed probe inside an aggregate [`Error::NoCompatibleServer`]-style
/// error, keeping the original error for `source()` traversal.
#[derive(Debug)]
#[non_exhaustive]
pub struct Attempt {
    /// Transport that was probed.
    pub transport: Transport,
    /// `host:port` that was probed.
    pub target: String,
    /// The underlying failure.
    pub error: Box<Error>,
}

impl fmt::Display for Attempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            self.transport, self.target, self.error
        )
    }
}

impl fmt::Display for Error {
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
                write_attempts(attempts, formatter)
            }
            Self::NoCompatibleJavaServer {
                host,
                java_port,
                attempts,
            } => {
                write!(
                    formatter,
                    "no compatible Java server found at {host} (JE:{java_port})"
                )?;
                write_attempts(attempts, formatter)
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
                write_attempts(attempts, formatter)
            }
        }
    }
}

fn write_attempts(attempts: &[Attempt], formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    if attempts.is_empty() {
        return Ok(());
    }
    let messages: Vec<String> = attempts.iter().map(ToString::to_string).collect();
    write!(formatter, ": {}", messages.join("; "))
}

impl std::error::Error for Error {
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
            | Self::NoCompatibleJavaServer { .. }
            | Self::NoCompatibleBedrockServer { .. } => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(error: std::str::Utf8Error) -> Self {
        Self::InvalidUtf8(error)
    }
}
