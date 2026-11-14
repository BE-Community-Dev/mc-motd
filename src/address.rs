use std::borrow::Cow;
use std::net::IpAddr;

use crate::error::Error;

const MAX_HOST_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// Normalized server address and edition-specific ports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

/// Parse a user-facing server address into a normalized host and edition ports.
///
/// The parser accepts `host`, `host:port`, and `[ipv6]:port`.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] when the input is empty, the host is
/// invalid, a bracketed IPv6 address is malformed, or a port is invalid.
pub fn parse_server_address(
    input: &str,
    default_java_port: u16,
    default_bedrock_port: u16,
) -> Result<ServerAddress, Error> {
    validate_port(default_java_port)?;
    validate_port(default_bedrock_port)?;

    let raw_host = trim_server_input(input)?;
    if let Some(host) = split_bracketed_host_without_port(raw_host.as_ref())? {
        validate_server_host(host)?;
        return Ok(ServerAddress {
            host: host.to_string(),
            java_port: default_java_port,
            bedrock_port: default_bedrock_port,
            explicit_port: None,
        });
    }
    if let Some((host, port)) = split_host_port(raw_host.as_ref())? {
        validate_server_host(host)?;
        return Ok(ServerAddress {
            host: host.to_string(),
            java_port: port,
            bedrock_port: port,
            explicit_port: Some(port),
        });
    }

    let host = normalize_server_host(raw_host.as_ref())?;
    if let Some((host, port)) = split_host_port(host.as_ref())? {
        validate_server_host(host)?;
        return Ok(ServerAddress {
            host: host.to_string(),
            java_port: port,
            bedrock_port: port,
            explicit_port: Some(port),
        });
    }

    Ok(ServerAddress {
        host: host.into_owned(),
        java_port: default_java_port,
        bedrock_port: default_bedrock_port,
        explicit_port: None,
    })
}

/// Normalize and validate a server host.
///
/// This trims a trailing DNS root dot, then validates the remaining hostname or
/// IP literal.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the input is empty or not a valid
/// hostname, IPv4 address, or IPv6 address without a port.
pub fn normalize_server_host(input: &str) -> Result<Cow<'_, str>, Error> {
    let value = trim_server_input(input)?;
    let value = value.as_ref().trim_end_matches('.');

    validate_server_host(value)?;

    if value == input {
        Ok(Cow::Borrowed(input))
    } else {
        Ok(Cow::Owned(value.to_string()))
    }
}

fn trim_server_input(input: &str) -> Result<Cow<'_, str>, Error> {
    let value = input.trim();
    if value.is_empty() {
        return Err(Error::InvalidInput("server host is empty"));
    }

    if value == input {
        Ok(Cow::Borrowed(input))
    } else {
        Ok(Cow::Owned(value.to_string()))
    }
}

/// Validate a normalized server host.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the host is empty, too long, contains
/// whitespace or control characters, contains an unbracketed port, or has an
/// invalid DNS label.
pub fn validate_server_host(host: &str) -> Result<(), Error> {
    if host.is_empty() {
        return Err(Error::InvalidInput("server host is empty"));
    }
    if host.len() > MAX_HOST_LEN {
        return Err(Error::InvalidInput("server host is too long"));
    }
    if host
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(Error::InvalidInput("server host contains whitespace"));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    if host.contains(':') {
        return Err(Error::InvalidInput(
            "IPv6 addresses with ports must use [addr]:port",
        ));
    }

    for label in host.split('.') {
        validate_dns_label(label)?;
    }

    Ok(())
}

/// Validate a TCP or UDP port number.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] when the port is zero.
pub fn validate_port(port: u16) -> Result<(), Error> {
    if port == 0 {
        return Err(Error::InvalidInput("server port must be greater than 0"));
    }
    Ok(())
}

/// Return whether a host can have Java Edition SRV records.
#[must_use]
pub fn can_query_srv(host: &str) -> bool {
    host.parse::<IpAddr>().is_err() && !host.contains(':')
}

/// Build the Java Edition SRV lookup name for a host.
#[must_use]
pub fn minecraft_srv_name(host: &str) -> String {
    format!("_minecraft._tcp.{}.", host.trim_end_matches('.'))
}

/// Normalize an SRV target hostname by removing the trailing DNS root dot.
#[must_use]
pub fn normalize_srv_target(target: &str) -> String {
    target.trim_end_matches('.').to_string()
}

fn split_bracketed_host_without_port(host: &str) -> Result<Option<&str>, Error> {
    let Some(rest) = host.strip_prefix('[') else {
        return Ok(None);
    };
    let Some((address, suffix)) = rest.split_once(']') else {
        return Err(Error::InvalidInput(
            "IPv6 address is missing closing bracket",
        ));
    };
    if suffix.is_empty() {
        return Ok(Some(address));
    }
    Ok(None)
}

fn split_host_port(host: &str) -> Result<Option<(&str, u16)>, Error> {
    if let Some(rest) = host.strip_prefix('[') {
        let Some((address, suffix)) = rest.split_once(']') else {
            return Err(Error::InvalidInput(
                "IPv6 address is missing closing bracket",
            ));
        };
        validate_server_host(address)?;
        if suffix.is_empty() {
            return Ok(None);
        }
        let Some(port_text) = suffix.strip_prefix(':') else {
            return Err(Error::InvalidInput("invalid IPv6 address suffix"));
        };
        return parse_port(port_text).map(|port| Some((address, port)));
    }

    if host.matches(':').count() != 1 {
        return Ok(None);
    }

    let Some((name, port_text)) = host.rsplit_once(':') else {
        return Ok(None);
    };
    if name.is_empty() || port_text.is_empty() {
        return Ok(None);
    }
    validate_server_host(name)?;
    parse_port(port_text).map(|port| Some((name, port)))
}

fn parse_port(value: &str) -> Result<u16, Error> {
    let port = value
        .parse::<u16>()
        .map_err(|_| Error::InvalidInput("server port is invalid"))?;
    validate_port(port)?;
    Ok(port)
}

fn validate_dns_label(label: &str) -> Result<(), Error> {
    if label.is_empty() {
        return Err(Error::InvalidInput("server host contains an empty label"));
    }
    if label.len() > MAX_LABEL_LEN {
        return Err(Error::InvalidInput("server host label is too long"));
    }
    let bytes = label.as_bytes();
    if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
        return Err(Error::InvalidInput(
            "server host label cannot start or end with '-'",
        ));
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(Error::InvalidInput(
            "server host contains invalid characters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_with_port() {
        let address =
            parse_server_address("play.example.com:25566", 25565, 19132).expect("valid address");

        assert_eq!(address.host, "play.example.com");
        assert_eq!(address.java_port, 25566);
        assert_eq!(address.bedrock_port, 25566);
        assert_eq!(address.explicit_port, Some(25566));
    }

    #[test]
    fn parses_ipv6_with_port() {
        let address = parse_server_address("[::1]:25565", 25565, 19132).expect("valid address");

        assert_eq!(address.host, "::1");
        assert_eq!(address.java_port, 25565);
        assert_eq!(address.bedrock_port, 25565);
        assert_eq!(address.explicit_port, Some(25565));
    }

    #[test]
    fn parses_bracketed_ipv6_without_port() {
        let address = parse_server_address("[::1]", 25565, 19132).expect("valid address");

        assert_eq!(address.host, "::1");
        assert_eq!(address.java_port, 25565);
        assert_eq!(address.bedrock_port, 19132);
        assert_eq!(address.explicit_port, None);
    }

    #[test]
    fn default_host_keeps_ports_implicit() {
        let address =
            parse_server_address("play.example.com", 25565, 19132).expect("valid address");

        assert_eq!(address.host, "play.example.com");
        assert_eq!(address.java_port, 25565);
        assert_eq!(address.bedrock_port, 19132);
        assert_eq!(address.explicit_port, None);
    }

    #[test]
    fn rejects_invalid_host() {
        let error = parse_server_address("bad host", 25565, 19132).expect_err("host should fail");

        assert!(matches!(error, Error::InvalidInput(_)));
    }

    #[test]
    fn rejects_url_like_input() {
        let error = parse_server_address("minecraft://play.example.com", 25565, 19132)
            .expect_err("URL-like input should fail");

        assert!(matches!(error, Error::InvalidInput(_)));
    }
}
