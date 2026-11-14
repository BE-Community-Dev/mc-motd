//! Structured error model for query and parsing failures.

use std::fmt;
use std::sync::Arc;

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
    ///
    /// The error is shared behind an [`Arc`] so cloning an [`Error`] keeps the
    /// original OS error code (`raw_os_error`) intact and costs one reference
    /// count instead of rebuilding the error.
    Io(Arc<std::io::Error>),
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
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Attempt {
    /// Transport that was probed.
    pub transport: Transport,
    /// `host:port` that was probed.
    pub target: String,
    /// The underlying failure.
    pub error: Box<Error>,
}

impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Self::ResolveFailed => Self::ResolveFailed,
            // `Arc` keeps the original OS error (kind, raw code, payload)
            // intact; rebuilding it would drop `raw_os_error()`.
            Self::Io(error) => Self::Io(Arc::clone(error)),
            Self::Timeout => Self::Timeout,
            Self::Dns(message) => Self::Dns(message.clone()),
            Self::InvalidInput(message) => Self::InvalidInput(message),
            Self::InvalidPacket(message) => Self::InvalidPacket(message),
            Self::InvalidUtf8(error) => Self::InvalidUtf8(*error),
            Self::HttpStatus(status) => Self::HttpStatus(*status),
            Self::Http(message) => Self::Http(message.clone()),
            Self::NoCompatibleServer {
                host,
                java_port,
                bedrock_port,
                attempts,
            } => Self::NoCompatibleServer {
                host: host.clone(),
                java_port: *java_port,
                bedrock_port: *bedrock_port,
                attempts: attempts.clone(),
            },
            Self::NoCompatibleJavaServer {
                host,
                java_port,
                attempts,
            } => Self::NoCompatibleJavaServer {
                host: host.clone(),
                java_port: *java_port,
                attempts: attempts.clone(),
            },
            Self::NoCompatibleBedrockServer {
                host,
                bedrock_port,
                attempts,
            } => Self::NoCompatibleBedrockServer {
                host: host.clone(),
                bedrock_port: *bedrock_port,
                attempts: attempts.clone(),
            },
        }
    }
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
    let mut attempts = attempts.iter();
    let Some(first) = attempts.next() else {
        return Ok(());
    };
    // Write straight into the formatter: no intermediate `Vec<String>` and no
    // joined copy of every failure message.
    write!(formatter, ": {first}")?;
    for attempt in attempts {
        write!(formatter, "; {attempt}")?;
    }
    Ok(())
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error.as_ref()),
            Self::InvalidUtf8(error) => Some(error),
            // Aggregate errors chain to the first probe failure so callers
            // can walk `source()` down to the original I/O or HTTP error.
            Self::NoCompatibleServer { attempts, .. }
            | Self::NoCompatibleJavaServer { attempts, .. }
            | Self::NoCompatibleBedrockServer { attempts, .. } => attempts
                .first()
                .map(|attempt| attempt.error.as_ref() as &(dyn std::error::Error + 'static)),
            Self::ResolveFailed
            | Self::Timeout
            | Self::Dns(_)
            | Self::InvalidInput(_)
            | Self::InvalidPacket(_)
            | Self::HttpStatus(_)
            | Self::Http(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(error: std::str::Utf8Error) -> Self {
        Self::InvalidUtf8(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloning_preserves_the_original_os_error() {
        let error = Error::from(std::io::Error::from_raw_os_error(10048));
        let cloned = error.clone();

        let Error::Io(cloned) = cloned else {
            panic!("expected an I/O error");
        };
        assert_eq!(
            cloned.raw_os_error(),
            Some(10048),
            "cloning must not drop the original OS error code"
        );
    }

    #[test]
    fn cloned_io_errors_share_the_same_allocation() {
        let error = Error::from(std::io::Error::from_raw_os_error(10048));
        let cloned = error.clone();

        let (Error::Io(original), Error::Io(cloned)) = (&error, &cloned) else {
            panic!("expected I/O errors");
        };
        assert!(
            Arc::ptr_eq(original, cloned),
            "a clone should bump the reference count, not rebuild the error"
        );
    }

    #[cfg(feature = "java")]
    #[test]
    fn aggregate_errors_format_every_attempt() {
        let error = Error::NoCompatibleJavaServer {
            host: "play.example.com".into(),
            java_port: 25_565,
            attempts: vec![
                Attempt {
                    transport: Transport::JavaTcp,
                    target: "play.example.com:25565".into(),
                    error: Box::new(Error::Timeout),
                },
                Attempt {
                    transport: Transport::JavaTcp,
                    target: "srv.example.com:25566".into(),
                    error: Box::new(Error::HttpStatus(503)),
                },
            ],
        };
        let rendered = error.to_string();

        assert!(rendered.contains("play.example.com:25565"));
        assert!(rendered.contains("srv.example.com:25566"));
        assert!(rendered.contains("; "), "attempts are separated by '; '");
    }
}
