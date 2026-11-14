//! Reusable query client with shared DNS handling, connection pooling, and a
//! bounded transport cache.
//!
//! [`Client`] owns the resources that make repeated queries cheap: the HTTP
//! connection pool for `NetherNet`, the transport-type cache, and the default
//! [`QueryOptions`]. It is [`Clone`] at the cost of a few reference counts, so
//! different async tasks can share one client. One-off configuration belongs
//! in [`QueryOptions`], passed to [`Client::query_with`].
//!
//! The crate-level free functions use a process-wide default client, so
//! [`crate::query`] behaves like a shared [`Client`] too.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::address::{self, ServerAddress};
#[cfg(feature = "bedrock")]
use crate::cache::TransportCache;
use crate::error::Result;
#[cfg(feature = "bedrock")]
use crate::status::Transport;
use crate::status::{Edition, Status};

/// Default Java Edition server port.
pub const DEFAULT_JAVA_PORT: u16 = 25_565;
/// Default Bedrock Edition server port.
pub const DEFAULT_BEDROCK_PORT: u16 = 19_132;
/// Default end-to-end query deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(3_000);
/// Default transport-cache capacity.
pub const DEFAULT_CACHE_CAPACITY: usize = 1024;
/// Budget cap for the optional SRV lookup inside automatic queries.
#[cfg(feature = "java")]
pub(crate) const SRV_LOOKUP_BUDGET: Duration = Duration::from_secs(1);
/// Default `NetherNet` response body cap.
#[cfg(feature = "nethernet")]
const DEFAULT_MAX_RESPONSE_BYTES: usize = crate::bedrock::nethernet::DEFAULT_MAX_RESPONSE_BYTES;
/// Default `NetherNet` response body cap when the feature is compiled out.
#[cfg(not(feature = "nethernet"))]
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Options for a single query.
///
/// Fields are public for direct construction and chainable methods exist for
/// builder-style usage. The client-level defaults live in
/// [`ClientBuilder`]; per-query options override them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueryOptions {
    /// Edition restriction: `None` probes every compiled edition, `Some`
    /// restricts the query to one edition.
    pub edition: Option<Edition>,
    /// Java Edition port used for direct probing.
    pub java_port: u16,
    /// Bedrock Edition port used for direct probing.
    pub bedrock_port: u16,
    /// End-to-end deadline for the whole query, including DNS and every
    /// probe. Not a per-stage timeout.
    pub timeout: Duration,
    /// Whether Java SRV lookup is attempted before direct probing.
    ///
    /// SRV lookup is a Java Edition concept, so a Bedrock-only query never
    /// consults SRV records.
    pub try_srv: bool,
    /// Whether a plain `http://` `NetherNet` endpoint on the same host and
    /// port may be tried after the primary `https://` attempt fails at the
    /// transport level. Receiving any HTTP status never triggers the
    /// fallback.
    pub allow_insecure_http: bool,
    /// Maximum accepted `NetherNet` response body size in bytes.
    pub max_response_bytes: usize,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            edition: None,
            java_port: DEFAULT_JAVA_PORT,
            bedrock_port: DEFAULT_BEDROCK_PORT,
            timeout: DEFAULT_TIMEOUT,
            try_srv: true,
            allow_insecure_http: true,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

impl QueryOptions {
    /// Restrict the query to one edition.
    #[must_use]
    pub fn edition(mut self, edition: Edition) -> Self {
        self.edition = Some(edition);
        self
    }

    /// Set the Java Edition port used for direct probing.
    #[must_use]
    pub fn java_port(mut self, port: u16) -> Self {
        self.java_port = port;
        self
    }

    /// Set the Bedrock Edition port used for direct probing.
    #[must_use]
    pub fn bedrock_port(mut self, port: u16) -> Self {
        self.bedrock_port = port;
        self
    }

    /// Set the end-to-end query deadline.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Enable or disable Java SRV lookup.
    #[must_use]
    pub fn try_srv(mut self, try_srv: bool) -> Self {
        self.try_srv = try_srv;
        self
    }

    /// Allow or forbid the plain-HTTP `NetherNet` fallback.
    #[must_use]
    pub fn allow_insecure_http(mut self, allowed: bool) -> Self {
        self.allow_insecure_http = allowed;
        self
    }

    /// Set the maximum accepted `NetherNet` response body size.
    #[must_use]
    pub fn max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = max_response_bytes;
        self
    }
}

/// Resources shared between the queries of one [`Client`].
#[allow(dead_code)]
pub(crate) struct Parts<'a> {
    /// Keeps the borrow lifetime alive even when no borrowed field is
    /// compiled in (e.g. Java-only builds without Bedrock).
    _lifetime: std::marker::PhantomData<&'a ()>,
    #[cfg(feature = "bedrock")]
    pub(crate) cache: &'a TransportCache,
    #[cfg(feature = "nethernet")]
    pub(crate) http: &'a reqwest::Client,
}

struct ClientInner {
    options: QueryOptions,
    #[cfg(feature = "bedrock")]
    cache: TransportCache,
    #[cfg(feature = "nethernet")]
    http: reqwest::Client,
}

/// Reusable query client.
///
/// A [`Client`] shares its DNS handling, `NetherNet` connection pool, and
/// transport cache across queries. Cloning is cheap and the clones share the
/// same resources.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

impl Default for Client {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl Client {
    /// Start building a client with default options.
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Query a server address across all compiled editions.
    ///
    /// The address may be `host`, `host:port`, or `[ipv6]:port`. Java and
    /// Bedrock probes run concurrently and the first compatible response
    /// wins.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] for invalid input, DNS failures, I/O failures, or
    /// when no compatible server answers.
    pub async fn query(&self, address: impl AsRef<str>) -> Result<Status> {
        let parsed = Self::parse(address, &self.inner.options)?;
        engine::full(&self.parts(), &parsed, &self.inner.options).await
    }

    /// Query a server address with explicit per-query options.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::query`].
    pub async fn query_with(
        &self,
        address: impl AsRef<str>,
        options: QueryOptions,
    ) -> Result<Status> {
        let parsed = Self::parse(address, &options)?;
        engine::full(&self.parts(), &parsed, &options).await
    }

    /// Query a Java Edition server address.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::query`],
    /// restricted to Java.
    #[cfg(feature = "java")]
    pub async fn java(&self, address: impl AsRef<str>) -> Result<Status> {
        let options = self.inner.options.clone().edition(Edition::Java);
        let parsed = Self::parse(address, &options)?;
        engine::java(&self.parts(), &parsed, &options).await
    }

    /// Query a Java Edition server address with explicit options.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::java`].
    #[cfg(feature = "java")]
    pub async fn java_with(
        &self,
        address: impl AsRef<str>,
        options: QueryOptions,
    ) -> Result<Status> {
        let options = options.edition(Edition::Java);
        let parsed = Self::parse(address, &options)?;
        engine::java(&self.parts(), &parsed, &options).await
    }

    /// Query a Bedrock Edition server address with automatic transport
    /// selection.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::query`],
    /// restricted to Bedrock.
    #[cfg(feature = "bedrock")]
    pub async fn bedrock(&self, address: impl AsRef<str>) -> Result<Status> {
        let options = self.inner.options.clone().edition(Edition::Bedrock);
        let parsed = Self::parse(address, &options)?;
        engine::bedrock(&self.parts(), &parsed, &options).await
    }

    /// Query a Bedrock Edition server address with explicit options.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::bedrock`].
    #[cfg(feature = "bedrock")]
    pub async fn bedrock_with(
        &self,
        address: impl AsRef<str>,
        options: QueryOptions,
    ) -> Result<Status> {
        let options = options.edition(Edition::Bedrock);
        let parsed = Self::parse(address, &options)?;
        engine::bedrock(&self.parts(), &parsed, &options).await
    }

    /// Query a Bedrock Edition server address over `RakNet` only.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::bedrock`],
    /// restricted to the `RakNet` transport.
    #[cfg(feature = "bedrock")]
    pub async fn raknet(&self, address: impl AsRef<str>) -> Result<Status> {
        let parsed = Self::parse(address, &self.inner.options)?;
        engine::raknet(&self.parts(), &parsed, &self.inner.options).await
    }

    /// Query a Bedrock Edition server address over `RakNet` only with explicit
    /// options.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::raknet`].
    #[cfg(feature = "bedrock")]
    pub async fn raknet_with(
        &self,
        address: impl AsRef<str>,
        options: QueryOptions,
    ) -> Result<Status> {
        let parsed = Self::parse(address, &options)?;
        engine::raknet(&self.parts(), &parsed, &options).await
    }

    /// Query a Bedrock Edition server address over `NetherNet` only.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::bedrock`],
    /// restricted to the `NetherNet` transport.
    #[cfg(feature = "nethernet")]
    pub async fn nethernet(&self, address: impl AsRef<str>) -> Result<Status> {
        let parsed = Self::parse(address, &self.inner.options)?;
        engine::nethernet(&self.parts(), &parsed, &self.inner.options).await
    }

    /// Query a Bedrock Edition server address over `NetherNet` only with
    /// explicit options.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::nethernet`].
    #[cfg(feature = "nethernet")]
    pub async fn nethernet_with(
        &self,
        address: impl AsRef<str>,
        options: QueryOptions,
    ) -> Result<Status> {
        let parsed = Self::parse(address, &options)?;
        engine::nethernet(&self.parts(), &parsed, &options).await
    }

    /// Return the cached transport for a Bedrock `host:port`, if present and
    /// fresh.
    #[cfg(feature = "bedrock")]
    #[must_use]
    pub fn cached_transport(&self, host: &str, port: u16) -> Option<Transport> {
        self.inner.cache.cached(host, port)
    }

    /// Clear this client's transport cache.
    #[cfg(feature = "bedrock")]
    pub fn clear_transport_cache(&self) {
        self.inner.cache.clear();
    }

    #[allow(clippy::unused_self)] // the borrowed resources differ per feature set
    fn parts(&self) -> Parts<'_> {
        Parts {
            _lifetime: std::marker::PhantomData,
            #[cfg(feature = "bedrock")]
            cache: &self.inner.cache,
            #[cfg(feature = "nethernet")]
            http: &self.inner.http,
        }
    }

    fn parse(address: impl AsRef<str>, options: &QueryOptions) -> Result<ServerAddress> {
        address::parse_server_address(address.as_ref(), options.java_port, options.bedrock_port)
    }
}

/// Builder for a [`Client`].
#[derive(Debug, Clone)]
pub struct ClientBuilder {
    options: QueryOptions,
    #[cfg(feature = "bedrock")]
    cache_capacity: usize,
}

#[allow(clippy::derivable_impls)] // not derivable when the bedrock feature adds cache_capacity
impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            options: QueryOptions::default(),
            #[cfg(feature = "bedrock")]
            cache_capacity: DEFAULT_CACHE_CAPACITY,
        }
    }
}

impl ClientBuilder {
    /// Set the default end-to-end query deadline.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = timeout;
        self
    }

    /// Set the default Java Edition port.
    #[must_use]
    pub fn java_port(mut self, port: u16) -> Self {
        self.options.java_port = port;
        self
    }

    /// Set the default Bedrock Edition port.
    #[must_use]
    pub fn bedrock_port(mut self, port: u16) -> Self {
        self.options.bedrock_port = port;
        self
    }

    /// Enable or disable Java SRV lookup by default.
    #[must_use]
    pub fn try_srv(mut self, try_srv: bool) -> Self {
        self.options.try_srv = try_srv;
        self
    }

    /// Allow or forbid the plain-HTTP `NetherNet` fallback by default.
    #[must_use]
    pub fn allow_insecure_http(mut self, allowed: bool) -> Self {
        self.options.allow_insecure_http = allowed;
        self
    }

    /// Set the default `NetherNet` response body cap.
    #[must_use]
    pub fn max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.options.max_response_bytes = max_response_bytes;
        self
    }

    /// Set the transport-cache capacity; zero disables caching.
    #[cfg(feature = "bedrock")]
    #[must_use]
    pub fn cache_capacity(mut self, capacity: usize) -> Self {
        self.cache_capacity = capacity;
        self
    }

    /// Build the client.
    #[must_use]
    pub fn build(self) -> Client {
        Client {
            inner: Arc::new(ClientInner {
                options: self.options,
                #[cfg(feature = "bedrock")]
                cache: TransportCache::new(self.cache_capacity),
                #[cfg(feature = "nethernet")]
                http: build_http_client(),
            }),
        }
    }
}

#[cfg(feature = "nethernet")]
fn build_http_client() -> reqwest::Client {
    crate::bedrock::nethernet::build_http_client()
}

static DEFAULT_CLIENT: OnceLock<Client> = OnceLock::new();

pub(crate) fn default_client() -> &'static Client {
    DEFAULT_CLIENT.get_or_init(Client::default)
}

/// Internal query engine shared by the free functions and [`Client`].
pub(crate) mod engine {
    #[cfg(any(feature = "nethernet", all(feature = "java", feature = "bedrock")))]
    use std::future::Future;
    use std::net::IpAddr;
    use std::time::{Duration, Instant};

    use tokio::net::lookup_host;
    use tokio::time;

    #[cfg(all(feature = "java", feature = "bedrock"))]
    use super::SRV_LOOKUP_BUDGET;
    use super::{Parts, QueryOptions};
    #[cfg(all(feature = "java", feature = "bedrock"))]
    use crate::address;
    use crate::address::ServerAddress;
    #[cfg(feature = "bedrock")]
    use crate::bedrock;
    #[cfg(any(feature = "nethernet", all(feature = "java", feature = "bedrock")))]
    use crate::error::Attempt;
    use crate::error::{Error, Result};
    #[cfg(feature = "java")]
    use crate::java;
    #[cfg(all(feature = "java", feature = "bedrock"))]
    use crate::status::Transport;
    use crate::status::{Edition, Status};

    /// Run a full query: honor the edition restriction, then race or restrict.
    pub(crate) async fn full(
        parts: &Parts<'_>,
        address: &ServerAddress,
        options: &QueryOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started + options.timeout;
        let result = match options.edition {
            #[cfg(feature = "java")]
            Some(Edition::Java) => java::run(address, options, deadline).await,
            #[cfg(feature = "bedrock")]
            Some(Edition::Bedrock) => bedrock::run(parts, address, options, deadline).await,
            None => auto_probe(parts, address, options, deadline).await,
        };
        finish(started, result)
    }

    /// Run a Java-only query.
    #[cfg(feature = "java")]
    pub(crate) async fn java(
        parts: &Parts<'_>,
        address: &ServerAddress,
        options: &QueryOptions,
    ) -> Result<Status> {
        let _ = parts;
        let started = Instant::now();
        let deadline = started + options.timeout;
        finish(started, java::run(address, options, deadline).await)
    }

    /// Run a Bedrock-only query with automatic transport selection.
    #[cfg(feature = "bedrock")]
    pub(crate) async fn bedrock(
        parts: &Parts<'_>,
        address: &ServerAddress,
        options: &QueryOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started + options.timeout;
        finish(
            started,
            bedrock::run(parts, address, options, deadline).await,
        )
    }

    /// Run a forced `RakNet` query; the transport error surfaces directly.
    #[cfg(feature = "bedrock")]
    pub(crate) async fn raknet(
        parts: &Parts<'_>,
        address: &ServerAddress,
        options: &QueryOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started + options.timeout;
        let ips = resolve_probe_ips(&address.host, deadline).await?;
        let result = bedrock::raknet::probe(address, deadline, Some(ips[0]), parts).await;
        finish(started, result.map_err(|attempt| *attempt.error))
    }

    /// Run a forced `NetherNet` query; the transport error surfaces directly.
    #[cfg(feature = "nethernet")]
    pub(crate) async fn nethernet(
        parts: &Parts<'_>,
        address: &ServerAddress,
        options: &QueryOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started + options.timeout;
        let result = crate::bedrock::nethernet::probe(address, deadline, parts, options).await;
        finish(started, result.map_err(|attempt| *attempt.error))
    }

    /// Probe every compiled edition when no edition restriction is set.
    async fn auto_probe(
        parts: &Parts<'_>,
        address: &ServerAddress,
        options: &QueryOptions,
        deadline: Instant,
    ) -> Result<Status> {
        let _ = parts;
        #[cfg(all(feature = "java", feature = "bedrock"))]
        {
            return java_then_bedrock(parts, address, options, deadline).await;
        }
        #[cfg(all(feature = "java", not(feature = "bedrock")))]
        {
            return java::run(address, options, deadline).await;
        }
        #[cfg(all(not(feature = "java"), feature = "bedrock"))]
        {
            bedrock::run(parts, address, options, deadline).await
        }
    }

    /// Race the Java and Bedrock probes with one shared DNS resolution; Java
    /// is preferred on a tie, and SRV records stay authoritative for the Java
    /// side.
    #[cfg(all(feature = "java", feature = "bedrock"))]
    async fn java_then_bedrock(
        parts: &Parts<'_>,
        address: &ServerAddress,
        options: &QueryOptions,
        deadline: Instant,
    ) -> Result<Status> {
        let mut prefix_attempts: Vec<Attempt> = Vec::new();

        if options.try_srv && address::can_query_srv(&address.host) {
            // SRV is optional metadata: cap its budget so a slow or unbound
            // SRV path cannot starve the direct probes.
            let srv_budget = remaining(deadline)?.min(SRV_LOOKUP_BUDGET);
            match java::resolve_srv(&address.host, srv_budget).await {
                Ok(targets) if !targets.is_empty() => {
                    // SRV records are authoritative for the address, so the
                    // Java probe (SRV targets first, then direct) stays
                    // preferred even though the Bedrock probe runs concurrently.
                    let java_future = java::probe_srv_sequence(address, &targets, deadline);
                    let bedrock_future = bedrock::probe(address, deadline, parts, options, None);
                    return race_probe(java_future, bedrock_future)
                        .await
                        .map_err(|attempts| Error::NoCompatibleServer {
                            host: address.host.clone(),
                            java_port: address.java_port,
                            bedrock_port: address.bedrock_port,
                            attempts,
                        });
                }
                Ok(_) => prefix_attempts.push(Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(Error::Dns("no SRV records found".to_string())),
                }),
                Err(error) => prefix_attempts.push(Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(error),
                }),
            }
        }

        // Resolve the host once and share the addresses between the Java and
        // Bedrock direct probes instead of looking it up twice. A shared
        // resolution failure must not kill the Bedrock race: NetherNet uses
        // its own resolver stack and RakNet resolves lazily.
        let resolved = resolve_probe_ips(&address.host, deadline).await;
        let shared_ip = resolved.as_ref().ok().and_then(|ips| ips.first().copied());

        let java_future = async move {
            match resolved {
                Ok(ips) => java::probe_direct(address, &ips, deadline).await,
                Err(error) => Err(vec![Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(error),
                }]),
            }
        };
        let bedrock_future = bedrock::probe(address, deadline, parts, options, shared_ip);
        race_probe(java_future, bedrock_future)
            .await
            .map_err(|attempts| {
                let mut all = std::mem::take(&mut prefix_attempts);
                all.extend(attempts);
                Error::NoCompatibleServer {
                    host: address.host.clone(),
                    java_port: address.java_port,
                    bedrock_port: address.bedrock_port,
                    attempts: all,
                }
            })
    }

    /// Race two probes that run concurrently, preferring the first future.
    ///
    /// Both futures are polled from the start, so the fallback probe is
    /// already running while the preferred probe is awaited. When the
    /// preferred probe fails, the fallback result is usually ready
    /// immediately. Attempt errors are only collected when both probes fail,
    /// keeping the success path allocation-free.
    #[cfg(any(feature = "nethernet", all(feature = "java", feature = "bedrock")))]
    pub(crate) async fn race_probe<P, F>(preferred: P, fallback: F) -> Result<Status, Vec<Attempt>>
    where
        P: Future<Output = Result<Status, Vec<Attempt>>>,
        F: Future<Output = Result<Status, Vec<Attempt>>>,
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

    /// Resolve a host once and return its addresses, IPv4 first.
    pub(crate) async fn resolve_probe_ips(
        host: &str,
        deadline: Instant,
    ) -> Result<Vec<IpAddr>, Error> {
        let mut resolved = time::timeout(remaining(deadline)?, lookup_host((host, 0)))
            .await
            .map_err(|_| Error::Timeout)??;
        let mut ips: Vec<IpAddr> = resolved.by_ref().map(|addr| addr.ip()).collect();
        // Prefer IPv4 addresses; they are the most reliably reachable path for
        // both Java TCP and Bedrock UDP probes.
        ips.sort_by_key(|ip| i32::from(!ip.is_ipv4()));
        if ips.is_empty() {
            Err(Error::ResolveFailed)
        } else {
            Ok(ips)
        }
    }

    /// Compute the time left before `deadline`, erroring when it has passed.
    pub(crate) fn remaining(deadline: Instant) -> Result<Duration, Error> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(Error::Timeout)
        } else {
            Ok(remaining)
        }
    }

    /// Stamp the end-to-end elapsed time on a successful result.
    fn finish(started: Instant, result: Result<Status>) -> Result<Status> {
        result.map(|mut status| {
            status.elapsed = started.elapsed();
            status
        })
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::error::Error;

    const MOCK_MOTD_TEXT: &str =
        "MCPE;RakNet Mock;589;1.21.100;3;20;12345;Line2;Survival;1;19132;19133;";

    /// Build a `RakNet` unconnected pong packet carrying the MOTD text.
    fn build_pong(text: &str) -> Vec<u8> {
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
    #[cfg(feature = "bedrock")]
    async fn spawn_pong_mock() -> SocketAddr {
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
                let pong = build_pong(MOCK_MOTD_TEXT);
                let _ = socket.send_to(&pong, peer).await;
            }
        });
        address
    }

    #[cfg(all(feature = "java", feature = "bedrock"))]
    #[tokio::test]
    async fn client_race_returns_bedrock_when_java_absent() {
        let client = Client::builder().timeout(Duration::from_secs(3)).build();
        let mock = spawn_pong_mock().await;
        let address = format!("{}:{}", mock.ip(), mock.port());

        let status = client
            .query(&address)
            .await
            .expect("raknet mock should win the race");

        assert_eq!(status.edition(), Some(Edition::Bedrock));
        assert_eq!(status.transport(), Some(Transport::RakNetUdp));
        assert_eq!(status.motd, "RakNet Mock");
        assert!(status.elapsed > Duration::ZERO);
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn bedrock_query_caches_transport() {
        let client = Client::builder().timeout(Duration::from_secs(3)).build();
        let mock = spawn_pong_mock().await;
        let host = mock.ip().to_string();
        let port = mock.port();

        let status = client
            .bedrock(format!("{host}:{port}"))
            .await
            .expect("raknet mock should answer");
        assert_eq!(status.transport(), Some(Transport::RakNetUdp));
        assert_eq!(
            client.cached_transport(&host, port),
            Some(Transport::RakNetUdp),
            "successful raknet transport should be cached"
        );

        let second = client
            .bedrock(format!("{host}:{port}"))
            .await
            .expect("cached transport query should answer");
        assert_eq!(second.transport(), Some(Transport::RakNetUdp));

        client.clear_transport_cache();
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn end_to_end_deadline_bounds_the_whole_query() {
        let client = Client::builder()
            .timeout(Duration::from_millis(250))
            .build();

        // Port 1 on the loopback interface is virtually always closed.
        let started = std::time::Instant::now();
        let error = client
            .query("127.0.0.1:1")
            .await
            .expect_err("closed port should fail");

        assert!(matches!(
            error,
            Error::Timeout | Error::NoCompatibleServer { .. }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the deadline must bound the whole query, took {:?}",
            started.elapsed()
        );
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn query_with_can_restrict_the_edition() {
        let client = Client::builder().timeout(Duration::from_secs(3)).build();
        let mock = spawn_pong_mock().await;
        let address = format!("{}:{}", mock.ip(), mock.port());

        let status = client
            .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
            .await
            .expect("bedrock-restricted query should answer");

        assert_eq!(status.edition(), Some(Edition::Bedrock));
    }
}
