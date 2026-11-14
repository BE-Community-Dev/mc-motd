//! Bedrock Edition status queries.
//!
//! [`query`] automatically selects the transport: the `RakNet` and
//! `NetherNet` probes run concurrently and the faster transport wins, with
//! `NetherNet` preferred on ties. The transport that answers is cached per
//! `host:port` inside the calling [`crate::Client`], so later queries skip the
//! failing probe. For a forced transport use [`raknet::query`] or
//! [`nethernet::query`].

use std::time::Instant;

#[cfg(feature = "nethernet")]
use tokio::time;

use crate::address::ServerAddress;
use crate::client::{Parts, QueryOptions, ResolvedOptions};
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
    crate::client::default_client()?.bedrock(address).await
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
    crate::client::default_client()?
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
    resolved: &ResolvedOptions,
    ips: &[std::net::IpAddr],
) -> Result<Status, Vec<Attempt>> {
    #[cfg(not(feature = "nethernet"))]
    {
        let _ = resolved;
        return raknet::probe(address, deadline, ips, parts)
            .await
            .map_err(|attempt| vec![attempt]);
    }

    #[cfg(feature = "nethernet")]
    probe_with_nethernet(address, deadline, parts, resolved, ips).await
}

/// Transport selection with `NetherNet` available.
#[cfg(feature = "nethernet")]
async fn probe_with_nethernet(
    address: &ServerAddress,
    deadline: Instant,
    parts: &Parts<'_>,
    resolved: &ResolvedOptions,
    ips: &[std::net::IpAddr],
) -> Result<Status, Vec<Attempt>> {
    let host = &address.host;
    let port = address.bedrock_port;
    match parts.cache.cached(host, port) {
        Some(Transport::NetherNetHttp) => {
            // Wait for the cached transport through a short grace window; if
            // it has not answered by then (a server may have switched away
            // from it), race it against the other transport so the cache
            // cannot turn into a serial timeout.
            let cached = async {
                nethernet::probe(address, deadline, parts, resolved, false)
                    .await
                    .map_err(|attempt| vec![attempt])
            };
            tokio::pin!(cached);
            match time::timeout(crate::client::CACHE_GRACE, &mut cached).await {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(mut first_attempts)) => {
                    let second = raknet::probe(address, deadline, ips, parts).await;
                    match second {
                        Ok(status) => Ok(status),
                        Err(second_attempt) => {
                            first_attempts.push(second_attempt);
                            Err(first_attempts)
                        }
                    }
                }
                Err(_) => {
                    let fallback = async {
                        raknet::probe(address, deadline, ips, parts)
                            .await
                            .map_err(|attempt| vec![attempt])
                    };
                    crate::client::engine::race_probe(&mut cached, fallback).await
                }
            }
        }
        Some(Transport::RakNetUdp) => {
            let cached = async {
                raknet::probe(address, deadline, ips, parts)
                    .await
                    .map_err(|attempt| vec![attempt])
            };
            tokio::pin!(cached);
            match time::timeout(crate::client::CACHE_GRACE, &mut cached).await {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(mut first_attempts)) => {
                    let second = nethernet::probe(address, deadline, parts, resolved, true).await;
                    match second {
                        Ok(status) => Ok(status),
                        Err(second_attempt) => {
                            first_attempts.push(second_attempt);
                            Err(first_attempts)
                        }
                    }
                }
                Err(_) => {
                    let fallback = async {
                        nethernet::probe(address, deadline, parts, resolved, true)
                            .await
                            .map_err(|attempt| vec![attempt])
                    };
                    crate::client::engine::race_probe(&mut cached, fallback).await
                }
            }
        }
        _ => {
            let nether_future = async {
                nethernet::probe(address, deadline, parts, resolved, true)
                    .await
                    .map_err(|attempt| vec![attempt])
            };
            let raknet_future = async {
                raknet::probe(address, deadline, ips, parts)
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
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status> {
    #[cfg(not(feature = "nethernet"))]
    let _ = resolved;
    probe(address, deadline, parts, resolved, &[])
        .await
        .map_err(|attempts| Error::NoCompatibleBedrockServer {
            host: address.host.clone(),
            bedrock_port: address.bedrock_port,
            attempts,
        })
}
