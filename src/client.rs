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
/// Minimum budget a single address attempt gets during multi-address
/// failover, so a large candidate list still gives every address a usable
/// slice of the shared deadline.
pub(crate) const MIN_ATTEMPT_BUDGET: Duration = Duration::from_millis(250);
/// How long a cache-hit transport is waited for exclusively before the other
/// transport joins the race as a parallel fallback.
#[cfg(feature = "nethernet")]
pub(crate) const CACHE_GRACE: Duration = Duration::from_millis(600);
/// Budget cap for the optional SRV lookup inside automatic queries.
#[cfg(feature = "java")]
pub(crate) const SRV_LOOKUP_BUDGET: Duration = Duration::from_secs(1);
/// Default `NetherNet` response body cap.
#[cfg(feature = "nethernet")]
const DEFAULT_MAX_RESPONSE_BYTES: usize = crate::bedrock::nethernet::DEFAULT_MAX_RESPONSE_BYTES;
/// Default `NetherNet` response body cap when the feature is compiled out.
#[cfg(not(feature = "nethernet"))]
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Default Java status packet cap (512 KiB; favicon-heavy responses stay well
/// below this).
pub const DEFAULT_MAX_JAVA_STATUS_BYTES: usize = 512 * 1024;

/// Per-query option overrides.
///
/// Every field is an override: `None` (the default) inherits the value from
/// the [`Client`] the query runs on, `Some` replaces it for this single
/// query. Build values through the chainable methods or with struct
/// update syntax — direct assignment is visible because the fields themselves
/// carry the override state.
///
/// The client-level defaults live in [`ClientBuilder`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueryOptions {
    /// Edition restriction: `None` probes every compiled edition, `Some`
    /// restricts the query to one edition.
    pub edition: Option<Edition>,
    /// Java Edition port used for direct probing.
    pub java_port: Option<u16>,
    /// Bedrock Edition port used for direct probing.
    pub bedrock_port: Option<u16>,
    /// End-to-end deadline for the whole query, including DNS and every
    /// probe. Not a per-stage timeout.
    pub timeout: Option<Duration>,
    /// Whether Java SRV lookup is attempted before direct probing.
    ///
    /// SRV lookup is a Java Edition concept, so a Bedrock-only query never
    /// consults SRV records.
    pub try_srv: Option<bool>,
    /// Whether a plain `http://` `NetherNet` endpoint on the same host and
    /// port may be tried after the primary `https://` attempt fails at the
    /// transport level. Receiving any HTTP status never triggers the
    /// fallback.
    ///
    /// Defaults to disabled: a plain HTTP answer is not authenticated, so
    /// callers who explicitly want NetherNet-over-HTTP compatibility must
    /// opt in here (or on the client builder).
    pub allow_insecure_http: Option<bool>,
    /// Maximum accepted `NetherNet` response body size in bytes.
    pub max_response_bytes: Option<usize>,
    /// Maximum accepted Java status packet size in bytes.
    pub max_java_status_bytes: Option<usize>,
}

/// Fully resolved options used inside the query engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedOptions {
    pub(crate) edition: Option<Edition>,
    pub(crate) java_port: u16,
    pub(crate) bedrock_port: u16,
    pub(crate) timeout: Duration,
    pub(crate) try_srv: bool,
    pub(crate) allow_insecure_http: bool,
    pub(crate) max_response_bytes: usize,
    pub(crate) max_java_status_bytes: usize,
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
        self.java_port = Some(port);
        self
    }

    /// Set the Bedrock Edition port used for direct probing.
    #[must_use]
    pub fn bedrock_port(mut self, port: u16) -> Self {
        self.bedrock_port = Some(port);
        self
    }

    /// Set the end-to-end query deadline.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Enable or disable Java SRV lookup.
    #[must_use]
    pub fn try_srv(mut self, try_srv: bool) -> Self {
        self.try_srv = Some(try_srv);
        self
    }

    /// Allow or forbid the plain-HTTP `NetherNet` fallback.
    #[must_use]
    pub fn allow_insecure_http(mut self, allowed: bool) -> Self {
        self.allow_insecure_http = Some(allowed);
        self
    }

    /// Set the maximum accepted `NetherNet` response body size.
    #[must_use]
    pub fn max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = Some(max_response_bytes);
        self
    }

    /// Set the maximum accepted Java status packet size.
    #[must_use]
    pub fn max_java_status_bytes(mut self, max_java_status_bytes: usize) -> Self {
        self.max_java_status_bytes = Some(max_java_status_bytes);
        self
    }

    /// Resolve these options over `base` (the client defaults): every `None`
    /// field inherits the base value, every `Some` field overrides it.
    pub(crate) fn resolve_over(&self, base: &QueryOptions) -> ResolvedOptions {
        ResolvedOptions {
            edition: self.edition.or(base.edition),
            java_port: self
                .java_port
                .or(base.java_port)
                .unwrap_or(DEFAULT_JAVA_PORT),
            bedrock_port: self
                .bedrock_port
                .or(base.bedrock_port)
                .unwrap_or(DEFAULT_BEDROCK_PORT),
            timeout: self.timeout.or(base.timeout).unwrap_or(DEFAULT_TIMEOUT),
            try_srv: self.try_srv.or(base.try_srv).unwrap_or(true),
            // Insecure HTTP stays disabled unless a builder or a per-query
            // override explicitly allows it.
            allow_insecure_http: self
                .allow_insecure_http
                .or(base.allow_insecure_http)
                .unwrap_or(false),
            max_response_bytes: self
                .max_response_bytes
                .or(base.max_response_bytes)
                .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
            max_java_status_bytes: self
                .max_java_status_bytes
                .or(base.max_java_status_bytes)
                .unwrap_or(DEFAULT_MAX_JAVA_STATUS_BYTES),
        }
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
    /// SRV resolver shared by this client's Java queries (also holds the
    /// background task hickory needs).
    #[cfg(feature = "java")]
    pub(crate) resolver: hickory_resolver::TokioResolver,
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
        let resolved = self.inner.options.resolve_over(&QueryOptions::default());
        let parsed = Self::parse(address, &resolved)?;
        #[cfg(feature = "java")]
        let status = engine::full(&self.parts(), &parsed, &resolved, self.srv_resolver()).await;
        #[cfg(not(feature = "java"))]
        let status = engine::full(&self.parts(), &parsed, &resolved).await;
        status
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
        // Per-query options layer on top of the client defaults: `None`
        // fields inherit the builder values.
        let resolved = options.resolve_over(&self.inner.options);
        let parsed = Self::parse(address, &resolved)?;
        #[cfg(feature = "java")]
        let full_result =
            engine::full(&self.parts(), &parsed, &resolved, self.srv_resolver()).await;
        #[cfg(not(feature = "java"))]
        let full_result = engine::full(&self.parts(), &parsed, &resolved).await;
        full_result
    }

    /// Query a Java Edition server address.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::query`],
    /// restricted to Java.
    #[cfg(feature = "java")]
    pub async fn java(&self, address: impl AsRef<str>) -> Result<Status> {
        let resolved = self
            .inner
            .options
            .clone()
            .edition(Edition::Java)
            .resolve_over(&QueryOptions::default());
        let parsed = Self::parse(address, &resolved)?;
        engine::java(&self.parts(), &parsed, &resolved, self.srv_resolver()).await
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
        let resolved = options
            .edition(Edition::Java)
            .resolve_over(&self.inner.options);
        let parsed = Self::parse(address, &resolved)?;
        engine::java(&self.parts(), &parsed, &resolved, self.srv_resolver()).await
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
        let resolved = self
            .inner
            .options
            .clone()
            .edition(Edition::Bedrock)
            .resolve_over(&QueryOptions::default());
        let parsed = Self::parse(address, &resolved)?;
        engine::bedrock(&self.parts(), &parsed, &resolved).await
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
        let resolved = options
            .edition(Edition::Bedrock)
            .resolve_over(&self.inner.options);
        let parsed = Self::parse(address, &resolved)?;
        engine::bedrock(&self.parts(), &parsed, &resolved).await
    }

    /// Query a Bedrock Edition server address over `RakNet` only.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::bedrock`],
    /// restricted to the `RakNet` transport.
    #[cfg(feature = "bedrock")]
    pub async fn raknet(&self, address: impl AsRef<str>) -> Result<Status> {
        let resolved = self.inner.options.resolve_over(&QueryOptions::default());
        let parsed = Self::parse(address, &resolved)?;
        engine::raknet(&self.parts(), &parsed, &resolved).await
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
        let resolved = options.resolve_over(&self.inner.options);
        let parsed = Self::parse(address, &resolved)?;
        engine::raknet(&self.parts(), &parsed, &resolved).await
    }

    /// Query a Bedrock Edition server address over `NetherNet` only.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error`] under the same conditions as [`Client::bedrock`],
    /// restricted to the `NetherNet` transport.
    #[cfg(feature = "nethernet")]
    pub async fn nethernet(&self, address: impl AsRef<str>) -> Result<Status> {
        let resolved = self.inner.options.resolve_over(&QueryOptions::default());
        let parsed = Self::parse(address, &resolved)?;
        engine::nethernet(&self.parts(), &parsed, &resolved).await
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
        let resolved = options.resolve_over(&self.inner.options);
        let parsed = Self::parse(address, &resolved)?;
        engine::nethernet(&self.parts(), &parsed, &resolved).await
    }

    /// Shared SRV resolver used by this client's Java queries.
    #[cfg(feature = "java")]
    pub(crate) fn srv_resolver(&self) -> &hickory_resolver::TokioResolver {
        &self.inner.resolver
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

    fn parse(address: impl AsRef<str>, resolved: &ResolvedOptions) -> Result<ServerAddress> {
        address::parse_server_address(address.as_ref(), resolved.java_port, resolved.bedrock_port)
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
        self.options.timeout = Some(timeout);
        self
    }

    /// Set the default Java Edition port.
    #[must_use]
    pub fn java_port(mut self, port: u16) -> Self {
        self.options.java_port = Some(port);
        self
    }

    /// Set the default Bedrock Edition port.
    #[must_use]
    pub fn bedrock_port(mut self, port: u16) -> Self {
        self.options.bedrock_port = Some(port);
        self
    }

    /// Enable or disable Java SRV lookup by default.
    #[must_use]
    pub fn try_srv(mut self, try_srv: bool) -> Self {
        self.options.try_srv = Some(try_srv);
        self
    }

    /// Allow or forbid the plain-HTTP `NetherNet` fallback by default.
    #[must_use]
    pub fn allow_insecure_http(mut self, allowed: bool) -> Self {
        self.options.allow_insecure_http = Some(allowed);
        self
    }

    /// Set the default `NetherNet` response body cap.
    #[must_use]
    pub fn max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.options.max_response_bytes = Some(max_response_bytes);
        self
    }

    /// Set the maximum accepted Java status packet size.
    #[must_use]
    pub fn max_java_status_bytes(mut self, max_java_status_bytes: usize) -> Self {
        self.options.max_java_status_bytes = Some(max_java_status_bytes);
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
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Http`] when the `NetherNet` HTTP client
    /// cannot be constructed, instead of silently substituting a differently
    /// configured client.
    pub fn build(self) -> std::result::Result<Client, crate::error::Error> {
        #[cfg(feature = "nethernet")]
        let http = crate::bedrock::nethernet::build_http_client()?;
        #[cfg(feature = "java")]
        let resolver = hickory_resolver::Resolver::builder_tokio()
            .map_err(|error| crate::error::Error::Dns(error.to_string()))?
            .build()
            .map_err(|error| crate::error::Error::Dns(error.to_string()))?;
        Ok(Client {
            inner: Arc::new(ClientInner {
                options: self.options,
                #[cfg(feature = "bedrock")]
                cache: TransportCache::new(self.cache_capacity),
                #[cfg(feature = "nethernet")]
                http,
                #[cfg(feature = "java")]
                resolver,
            }),
        })
    }
}

static DEFAULT_CLIENT: OnceLock<std::result::Result<Client, String>> = OnceLock::new();

pub(crate) fn default_client() -> std::result::Result<&'static Client, crate::error::Error> {
    DEFAULT_CLIENT
        .get_or_init(|| Client::builder().build().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|message| crate::error::Error::Http(message.clone()))
}

/// Internal query engine shared by the free functions and [`Client`].
pub(crate) mod engine {
    #[cfg(any(feature = "nethernet", all(feature = "java", feature = "bedrock")))]
    use std::future::Future;
    use std::net::IpAddr;
    use std::time::{Duration, Instant};

    use tokio::net::lookup_host;
    use tokio::time;

    use super::{Parts, ResolvedOptions};
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
        resolved: &ResolvedOptions,
        #[cfg(feature = "java")] srv_resolver: &hickory_resolver::TokioResolver,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        let result = match resolved.edition {
            #[cfg(feature = "java")]
            Some(Edition::Java) => java::run(address, resolved, deadline, srv_resolver).await,
            #[cfg(feature = "bedrock")]
            Some(Edition::Bedrock) => bedrock::run(parts, address, resolved, deadline).await,
            #[cfg(all(feature = "java", feature = "bedrock"))]
            None => auto_probe(parts, address, resolved, deadline, srv_resolver).await,
            #[cfg(all(feature = "java", not(feature = "bedrock")))]
            None => auto_probe(parts, address, resolved, deadline, srv_resolver).await,
            #[cfg(all(not(feature = "java"), feature = "bedrock"))]
            None => auto_probe(parts, address, resolved, deadline).await,
        };
        finish(started, result)
    }

    /// Run a Java-only query.
    #[cfg(feature = "java")]
    pub(crate) async fn java(
        parts: &Parts<'_>,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
        srv_resolver: &hickory_resolver::TokioResolver,
    ) -> Result<Status> {
        let _ = parts;
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        finish(
            started,
            java::run(address, resolved, deadline, srv_resolver).await,
        )
    }

    /// Run a Bedrock-only query with automatic transport selection.
    #[cfg(feature = "bedrock")]
    pub(crate) async fn bedrock(
        parts: &Parts<'_>,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        finish(
            started,
            bedrock::run(parts, address, resolved, deadline).await,
        )
    }

    /// Run a forced `RakNet` query; the transport error surfaces directly.
    #[cfg(feature = "bedrock")]
    pub(crate) async fn raknet(
        parts: &Parts<'_>,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        let ips = resolve_probe_ips(&address.host, deadline).await?;
        let result = bedrock::raknet::probe(address, deadline, &ips, parts).await;
        finish(started, result.map_err(|attempt| *attempt.error))
    }

    /// Run a forced `NetherNet` query; the transport error surfaces directly.
    #[cfg(feature = "nethernet")]
    pub(crate) async fn nethernet(
        parts: &Parts<'_>,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        let result =
            crate::bedrock::nethernet::probe(address, deadline, parts, resolved, true).await;
        finish(started, result.map_err(|attempt| *attempt.error))
    }

    /// Probe every compiled edition when no edition restriction is set.
    async fn auto_probe(
        parts: &Parts<'_>,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
        deadline: Instant,
        #[cfg(feature = "java")] srv_resolver: &hickory_resolver::TokioResolver,
    ) -> Result<Status> {
        let _ = parts;
        #[cfg(all(feature = "java", feature = "bedrock"))]
        {
            return java_then_bedrock(parts, address, resolved, deadline, srv_resolver).await;
        }
        #[cfg(all(feature = "java", not(feature = "bedrock")))]
        {
            return java::run(address, resolved, deadline, srv_resolver).await;
        }
        #[cfg(all(not(feature = "java"), feature = "bedrock"))]
        {
            bedrock::run(parts, address, resolved, deadline).await
        }
    }

    /// Race the Java and Bedrock probes. Both sides start immediately and
    /// manage their own DNS (Java: budget-capped SRV plus resolution; Bedrock:
    /// `NetherNet` on its own resolver stack, `RakNet` resolving lazily), so no
    /// serial SRV/DNS phase can delay a transport that is ready to answer.
    /// Java is preferred on a tie, and SRV records stay authoritative for the
    /// Java side.
    #[cfg(all(feature = "java", feature = "bedrock"))]
    async fn java_then_bedrock(
        parts: &Parts<'_>,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
        deadline: Instant,
        srv_resolver: &hickory_resolver::TokioResolver,
    ) -> Result<Status> {
        let java_future = async move {
            java::run(address, resolved, deadline, srv_resolver)
                .await
                .map_err(|error| match error {
                    Error::NoCompatibleJavaServer { attempts, .. } => attempts,
                    other => vec![Attempt {
                        transport: Transport::JavaTcp,
                        target: address.host.clone(),
                        error: Box::new(other),
                    }],
                })
        };
        let bedrock_future = async move {
            bedrock::run(parts, address, resolved, deadline)
                .await
                .map_err(|error| match error {
                    Error::NoCompatibleBedrockServer { attempts, .. } => attempts,
                    other => vec![Attempt {
                        transport: Transport::RakNetUdp,
                        target: format!("{}:{}", address.host, address.bedrock_port),
                        error: Box::new(other),
                    }],
                })
        };
        race_probe(java_future, bedrock_future)
            .await
            .map_err(|attempts| Error::NoCompatibleServer {
                host: address.host.clone(),
                java_port: address.java_port,
                bedrock_port: address.bedrock_port,
                attempts,
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

    /// Build a `RakNet` unconnected pong packet carrying the MOTD text and
    /// echoing the ping timestamp.
    fn build_pong(ping_time: [u8; 8], text: &str) -> Vec<u8> {
        let mut packet = Vec::with_capacity(35 + text.len());
        packet.push(0x1c);
        packet.extend_from_slice(&ping_time);
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
                let pong = build_pong(buffer[1..9].try_into().expect("8 bytes"), MOCK_MOTD_TEXT);
                let _ = socket.send_to(&pong, peer).await;
            }
        });
        address
    }

    #[cfg(all(feature = "java", feature = "bedrock"))]
    #[tokio::test]
    async fn client_race_returns_bedrock_when_java_absent() {
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");
        let mock = spawn_pong_mock().await;
        let address = format!("{}:{}", mock.ip(), mock.port());

        let status = client
            .query(&address)
            .await
            .expect("raknet mock should win the race");

        assert_eq!(status.edition(), Edition::Bedrock);
        assert_eq!(status.transport(), Transport::RakNetUdp);
        assert_eq!(status.motd, "RakNet Mock");
        assert!(status.elapsed > Duration::ZERO);
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn bedrock_query_caches_transport() {
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");
        let mock = spawn_pong_mock().await;
        let host = mock.ip().to_string();
        let port = mock.port();

        let status = client
            .bedrock(format!("{host}:{port}"))
            .await
            .expect("raknet mock should answer");
        assert_eq!(status.transport(), Transport::RakNetUdp);
        assert_eq!(
            client.cached_transport(&host, port),
            Some(Transport::RakNetUdp),
            "successful raknet transport should be cached"
        );

        let second = client
            .bedrock(format!("{host}:{port}"))
            .await
            .expect("cached transport query should answer");
        assert_eq!(second.transport(), Transport::RakNetUdp);

        client.clear_transport_cache();
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn end_to_end_deadline_bounds_the_whole_query() {
        let client = Client::builder()
            .timeout(Duration::from_millis(250))
            .build()
            .expect("test client should build");

        // Port 1 on the loopback interface is virtually always closed.
        let started = std::time::Instant::now();
        let error = client
            .query("127.0.0.1:1")
            .await
            .expect_err("closed port should fail");

        // The aggregate variant depends on the compiled edition set
        // (java-only, bedrock-only, or both).
        assert!(matches!(
            error,
            Error::Timeout
                | Error::NoCompatibleServer { .. }
                | Error::NoCompatibleJavaServer { .. }
                | Error::NoCompatibleBedrockServer { .. }
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
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");
        let mock = spawn_pong_mock().await;
        let address = format!("{}:{}", mock.ip(), mock.port());

        let status = client
            .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
            .await
            .expect("bedrock-restricted query should answer");

        assert_eq!(status.edition(), Edition::Bedrock);
    }

    #[test]
    fn query_with_resolves_over_client_defaults() {
        // The builder pins a java port; a per-query timeout must not reset it.
        let client = Client::builder()
            .java_port(25_599)
            .build()
            .expect("test client should build");
        let resolved = QueryOptions::default()
            .timeout(Duration::from_secs(7))
            .resolve_over(&client.inner.options);

        assert_eq!(resolved.timeout, Duration::from_secs(7));
        assert_eq!(resolved.java_port, 25_599);

        // Explicit per-query fields do override the builder.
        let resolved = QueryOptions::default()
            .java_port(25_600)
            .resolve_over(&client.inner.options);
        assert_eq!(resolved.java_port, 25_600);
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn aggregate_failures_keep_every_probe_attempt() {
        let client = Client::builder()
            .timeout(Duration::from_millis(400))
            .allow_insecure_http(false)
            .build()
            .expect("test client should build");

        // Port 1 on the loopback interface is virtually always closed, so
        // both Bedrock transports fail.
        let error = client
            .bedrock("127.0.0.1:1")
            .await
            .expect_err("closed port should fail");

        let attempts = match &error {
            Error::NoCompatibleBedrockServer { attempts, .. } => attempts,
            other => panic!("expected an aggregate error, got {other:?}"),
        };
        // Expect one attempt per compiled transport: both with NetherNet and
        // RakNet, one with RakNet only.
        #[cfg(feature = "nethernet")]
        {
            assert_eq!(
                attempts.len(),
                2,
                "both probe failures are kept, got {attempts:?}"
            );
        }
        #[cfg(not(feature = "nethernet"))]
        {
            assert_eq!(
                attempts.len(),
                1,
                "a single compiled transport yields a single attempt"
            );
        }
        // The aggregate error chains to the first probe failure.
        assert!(
            std::error::Error::source(&error).is_some(),
            "aggregate errors must connect to the original error chain"
        );
    }
}
