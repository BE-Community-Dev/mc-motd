//! Async Minecraft server status queries for Java Edition and Bedrock Edition.
//!
//! The crate is organized in three layers:
//!
//! 1. **One-liner queries.** [`query`] probes every compiled edition
//!    concurrently (Java TCP status plus, for Bedrock, both the legacy
//!    `RakNet` UDP ping and the `NetherNet` join endpoint used by Bedrock
//!    Edition 1.26.51+). The first compatible response wins; inside the
//!    Bedrock race the faster transport wins, with `NetherNet` preferred on
//!    ties. [`java::query`] and [`bedrock::query`] restrict the query to one
//!    edition, and [`bedrock::raknet::query`] / [`bedrock::nethernet::query`]
//!    force a specific transport.
//! 2. **Reusable clients.** [`Client`] shares DNS handling, the `NetherNet`
//!    connection pool, and a bounded transport cache across queries. It is
//!    cheap to clone, so async tasks can share one instance. Per-query
//!    configuration uses the chainable [`QueryOptions`].
//! 3. **Protocol codecs.** [`protocol`] exposes pure encoding and decoding of
//!    the Java status handshake, the `RakNet` ping/pong exchange, and the
//!    `NetherNet` join JSON — no sockets, no caching. Zero-copy paths live in
//!    [`bedrock::raknet::query_addr_into`] (pong into a caller-owned buffer)
//!    and [`protocol::bedrock::nethernet::parse_join_ref`] (borrowed `Cow`
//!    fields).
//!
//! The reported `elapsed` time is the end-to-end wall time of the whole
//! query. For `Transport::NetherNetHttp` this is HTTPS/HTTP signaling latency
//! and is *not* the game connection RTT; the game runs over WebRTC/UDP after
//! the join handshake.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use mc_motd::{Details, query};
//!
//! let status = query("play.example.com").await?;
//!
//! println!("transport={:?} elapsed={:?}", status.transport(), status.elapsed);
//! println!("{}", status.motd);
//! println!("{:?}", status.players);
//! match &status.details {
//!     Details::Java(details) => println!("{}", details.raw_json),
//!     Details::Bedrock(bedrock) => println!("{bedrock:?}"),
//!     // Non_exhaustive: future protocols fit without another breaking release.
//!     _ => {}
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Editions are isolatable: the `java` and `nethernet` cargo features (both on
//! by default) control what is compiled at all, so a Bedrock-only build can
//! drop Java and its DNS dependency entirely. See the README feature table.

#[cfg(not(any(feature = "java", feature = "bedrock")))]
compile_error!("mc-motd requires at least one of the `java` or `bedrock` features");

pub mod address;
#[cfg(feature = "bedrock")]
pub mod bedrock;
#[cfg(feature = "bedrock")]
pub mod cache;
pub mod client;
pub mod error;
#[cfg(feature = "java")]
pub mod java;
#[cfg(feature = "lan")]
pub mod lan;
pub mod protocol;
pub mod status;

pub use address::{
    ServerAddress, normalize_server_host, parse_server_address, validate_port, validate_server_host,
};
pub use client::{
    Client, DEFAULT_BEDROCK_PORT, DEFAULT_CACHE_CAPACITY, DEFAULT_JAVA_PORT, DEFAULT_TIMEOUT,
    QueryOptions,
};
pub use error::{Error, Result};
#[cfg(feature = "java")]
pub use status::JavaDetails;
#[cfg(feature = "nethernet")]
pub use status::NetherNetDetails;
#[cfg(feature = "bedrock")]
pub use status::{BedrockDetails, RakNetDetails};
pub use status::{Details, Edition, Players, Status, Transport, Version};

use crate::client::default_client;

/// Query a server address across every compiled edition.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`. Without an
/// explicit port, edition default ports apply. Java and Bedrock probes run
/// concurrently and the first compatible response wins.
///
/// This uses a process-wide default [`Client`]; for shared resources across
/// many queries build your own with [`Client::builder`].
///
/// # Errors
///
/// Returns [`Error`] for invalid input, DNS failures, I/O failures, or when
/// no compatible server answers.
pub async fn query(address: impl AsRef<str>) -> Result<Status> {
    default_client().query(address).await
}

/// Query a server address with explicit per-query options.
///
/// This uses a process-wide default [`Client`]; the options override the
/// client defaults for this single query.
///
/// # Errors
///
/// Returns [`Error`] under the same conditions as [`query`].
pub async fn query_with(address: impl AsRef<str>, options: QueryOptions) -> Result<Status> {
    default_client().query_with(address, options).await
}
