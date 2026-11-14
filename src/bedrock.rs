//! Bedrock Edition status queries.
//!
//! [`query`] automatically selects the transport: the `RakNet` and
//! `NetherNet` probes run concurrently and the faster transport wins, with
//! `NetherNet` preferred on ties. The transport that answers is cached per
//! `host:port` inside the calling [`crate::Client`], so later queries skip the
//! failing probe. For a forced transport use [`raknet::query`] or
//! [`nethernet::query`].

use std::time::Instant;

use crate::address::ServerAddress;
use crate::client::{Parts, QueryOptions};
use crate::error::{Attempt, Error, Result};
use crate::status::Status;
#[cfg(feature = "nethernet")]
use crate::status::Transport;

/// Legacy `RakNet` UDP status queries.
pub mod raknet;

/// `NetherNet` HTTPS/HTTP status queries.
#[cfg(feature = "nethernet")]
pub mod nethernet;

/// Query a Bedrock Edition server address with automatic transport selection.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`; the Bedrock
/// default port applies when no explicit port is present.
///
/// # Errors
///
/// Returns [`Error::NoCompatibleBedrockServer`] when neither compiled Bedrock
/// transport answers, and [`Error`] for invalid input, DNS, and I/O failures.
pub async fn query(address: impl AsRef<str>) -> Result<Status> {
    crate::client::default_client().bedrock(address).await
}

/// Query a Bedrock Edition server address with explicit options.
///
/// The `edition` field of `options` is ignored here; this function always
/// probes Bedrock.
///
/// # Errors
///
/// Returns [`Error`] under the same conditions as [`query`].
pub async fn query_with(address: impl AsRef<str>, options: QueryOptions) -> Result<Status> {
    crate::client::default_client()
        .bedrock_with(address, options)
        .await
}

/// Probe a Bedrock server, honoring the cached transport when one is known.
///
/// The cached transport is attempted first. When no transport is cached yet,
/// both compiled transports are raced concurrently: the faster transport wins
/// and `NetherNet` is preferred on ties, so third-party servers that answer
/// both protocols always get the quickest available answer.
pub(crate) async fn probe(
    address: &ServerAddress,
    deadline: Instant,
    parts: &Parts<'_>,
    options: &QueryOptions,
    resolved: Option<std::net::IpAddr>,
) -> Result<Status, Vec<Attempt>> {
    #[cfg(not(feature = "nethernet"))]
    let _ = options;
    #[cfg(not(feature = "nethernet"))]
    return raknet::probe(address, deadline, resolved, parts)
        .await
        .map_err(|attempt| vec![attempt]);

    #[cfg(feature = "nethernet")]
    probe_with_nethernet(address, deadline, parts, options, resolved).await
}

/// Transport selection with `NetherNet` available.
#[cfg(feature = "nethernet")]
async fn probe_with_nethernet(
    address: &ServerAddress,
    deadline: Instant,
    parts: &Parts<'_>,
    options: &QueryOptions,
    resolved: Option<std::net::IpAddr>,
) -> Result<Status, Vec<Attempt>> {
    let host = &address.host;
    let port = address.bedrock_port;
    match parts.cache.cached(host, port) {
        Some(Transport::NetherNetHttp) => {
            if let Ok(status) = nethernet::probe(address, deadline, parts, options).await {
                return Ok(status);
            }
            raknet::probe(address, deadline, resolved, parts)
                .await
                .map_err(|attempt| vec![attempt])
        }
        Some(Transport::RakNetUdp) => {
            if let Ok(status) = raknet::probe(address, deadline, resolved, parts).await {
                return Ok(status);
            }
            nethernet::probe(address, deadline, parts, options)
                .await
                .map_err(|attempt| vec![attempt])
        }
        _ => {
            let nether_future = async {
                nethernet::probe(address, deadline, parts, options)
                    .await
                    .map_err(|attempt| vec![attempt])
            };
            let raknet_future = async {
                raknet::probe(address, deadline, resolved, parts)
                    .await
                    .map_err(|attempt| vec![attempt])
            };
            crate::client::engine::race_probe(nether_future, raknet_future).await
        }
    }
}

/// Probe the Bedrock transports and wrap failures in the Bedrock-only error.
pub(crate) async fn run(
    parts: &Parts<'_>,
    address: &ServerAddress,
    options: &QueryOptions,
    deadline: Instant,
) -> Result<Status> {
    probe(address, deadline, parts, options, None)
        .await
        .map_err(|attempts| Error::NoCompatibleBedrockServer {
            host: address.host.clone(),
            bedrock_port: address.bedrock_port,
            attempts,
        })
}
