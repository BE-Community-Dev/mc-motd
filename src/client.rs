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

use std::collections::HashMap;
#[cfg(all(test, feature = "bedrock"))]
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::time;

use crate::address::{self, ServerAddress};
#[cfg(feature = "bedrock")]
use crate::cache::TransportCache;
use crate::error::{Error, Result};
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
#[derive(Clone)]
pub(crate) struct Parts {
    #[cfg(feature = "bedrock")]
    pub(crate) cache: Arc<TransportCache>,
    #[cfg(feature = "nethernet")]
    pub(crate) http: reqwest::Client,
    pub(crate) resolver: SrvResolver,
    /// Bounds concurrent probes for this client. Each engine entry point
    /// acquires exactly one permit for the whole query.
    pub(crate) in_flight: Arc<tokio::sync::Semaphore>,
}

struct ClientInner {
    options: QueryOptions,
    /// Merge concurrent identical full queries into one shared probe.
    merge_in_flight: bool,
    #[cfg(feature = "bedrock")]
    cache: Arc<TransportCache>,
    #[cfg(feature = "nethernet")]
    http: reqwest::Client,
    /// SRV resolver shared by this client's Java queries (also holds the
    /// background task hickory needs).
    pub(crate) resolver: SrvResolver,
    /// Bounds the number of probes running concurrently across this client.
    in_flight: Arc<tokio::sync::Semaphore>,
    /// Bounds the number of distinct merged flights queued or running. A slot
    /// is held from flight creation until its shared probe finishes, so a
    /// scan of many targets cannot accumulate unbounded detached tasks.
    queued: Arc<tokio::sync::Semaphore>,
    /// Merges concurrent identical queries into one shared probe when
    /// `merge_in_flight` was enabled on the builder.
    flights: Arc<Mutex<HashMap<String, Arc<Flight>>>>,
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
        if self.inner.merge_in_flight {
            let started = Instant::now();
            let deadline = started
                .checked_add(resolved.timeout)
                .ok_or(Error::InvalidInput("query timeout is too large"))?;
            let key = flight_key(&parsed, &resolved);
            return self
                .merged_probe(&key, started, deadline, &parsed, &resolved)
                .await;
        }
        engine::full(&self.parts(), &parsed, &resolved, self.srv_resolver()).await
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
        if self.inner.merge_in_flight {
            let started = Instant::now();
            let deadline = started
                .checked_add(resolved.timeout)
                .ok_or(Error::InvalidInput("query timeout is too large"))?;
            let key = flight_key(&parsed, &resolved);
            return self
                .merged_probe(&key, started, deadline, &parsed, &resolved)
                .await;
        }
        engine::full(&self.parts(), &parsed, &resolved, self.srv_resolver()).await
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
    #[must_use]
    pub(crate) fn srv_resolver(&self) -> &SrvResolver {
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
    fn parts(&self) -> Parts {
        Parts {
            #[cfg(feature = "bedrock")]
            cache: Arc::clone(&self.inner.cache),
            #[cfg(feature = "nethernet")]
            http: self.inner.http.clone(),
            #[allow(clippy::clone_on_copy, clippy::unit_arg)]
            resolver: self.inner.resolver.clone(),
            in_flight: Arc::clone(&self.inner.in_flight),
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
    merge_in_flight: bool,
    max_in_flight: Option<usize>,
    max_queued: Option<usize>,
}

#[allow(clippy::derivable_impls)] // not derivable when the bedrock feature adds cache_capacity
impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            options: QueryOptions::default(),
            #[cfg(feature = "bedrock")]
            cache_capacity: DEFAULT_CACHE_CAPACITY,
            merge_in_flight: false,
            max_in_flight: None,
            max_queued: None,
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

    /// Merge concurrent identical queries into one shared probe.
    ///
    /// When several callers query the same address with the same effective
    /// options and timeout at the same time, only one probe runs and every
    /// caller receives its result. A failed shared probe is shared too:
    /// waiters never re-probe on their own, so an unreachable server cannot
    /// turn one merged probe into a request storm. Every caller still stops
    /// at its own deadline.
    ///
    /// A caller that joins an existing flight shares the probe that is
    /// currently running; it does not get a fresh full timeout window. Its
    /// result still arrives before its own deadline because the shared probe's
    /// absolute deadline was fixed earlier.
    ///
    /// Use [`ClientBuilder::max_queued`] to bound how many distinct flights
    /// (and their detached tasks) may exist at once.
    ///
    /// Defaults to disabled.
    #[must_use]
    pub fn merge_in_flight(mut self, merge: bool) -> Self {
        self.merge_in_flight = merge;
        self
    }

    /// Bound the number of logical queries this client runs concurrently.
    ///
    /// One permit is held for a whole query — DNS, every transport probe, and
    /// the singleflight shared probe — and released when the query finishes or
    /// is cancelled. Callers queue inside their own query deadline. Merged
    /// waiters do not hold permits; only the task actually probing does.
    ///
    /// `None` (the default) leaves concurrency unbounded.
    ///
    /// # Errors
    ///
    /// [`ClientBuilder::build`] rejects `0` and values above
    /// [`tokio::sync::Semaphore::MAX_PERMITS`].
    #[must_use]
    pub fn max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = Some(max_in_flight);
        self
    }

    /// Bound the number of distinct merged flights queued or running.
    ///
    /// Only used when [`ClientBuilder::merge_in_flight`] is enabled. A flight
    /// holds one slot from creation until its shared probe finishes, so a
    /// client that scans many different targets cannot accumulate unbounded
    /// detached probe tasks and flight entries. Callers waiting on an existing
    /// flight do not consume a slot; a caller waiting for a free slot is
    /// bounded by its own query deadline.
    ///
    /// This is separate from [`ClientBuilder::max_in_flight`]: the queue bound
    /// limits how many probes may exist, the in-flight bound limits how many
    /// may execute network I/O at once.
    ///
    /// `None` (the default) leaves the queue unbounded.
    ///
    /// # Errors
    ///
    /// [`ClientBuilder::build`] rejects `0` and values above
    /// [`tokio::sync::Semaphore::MAX_PERMITS`].
    #[must_use]
    pub fn max_queued(mut self, max_queued: usize) -> Self {
        self.max_queued = Some(max_queued);
        self
    }

    /// Build the client.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::InvalidInput`] when `max_in_flight` or
    /// `max_queued` is zero or exceeds [`tokio::sync::Semaphore::MAX_PERMITS`],
    /// and [`crate::error::Error::Http`] when the `NetherNet` HTTP client
    /// cannot be constructed, instead of silently substituting a differently
    /// configured client.
    pub fn build(self) -> std::result::Result<Client, crate::error::Error> {
        if let Some(limit) = self.max_in_flight {
            if limit == 0 {
                return Err(Error::InvalidInput(
                    "max_in_flight must be greater than zero",
                ));
            }
            if limit > tokio::sync::Semaphore::MAX_PERMITS {
                return Err(Error::InvalidInput(
                    "max_in_flight exceeds the semaphore limit",
                ));
            }
        }
        if let Some(limit) = self.max_queued {
            if limit == 0 {
                return Err(Error::InvalidInput("max_queued must be greater than zero"));
            }
            if limit > tokio::sync::Semaphore::MAX_PERMITS {
                return Err(Error::InvalidInput(
                    "max_queued exceeds the semaphore limit",
                ));
            }
        }
        #[cfg(feature = "nethernet")]
        let http = crate::bedrock::nethernet::build_http_client()?;
        #[allow(clippy::let_unit_value)]
        let resolver = build_srv_resolver()?;
        Ok(Client {
            inner: Arc::new(ClientInner {
                options: self.options,
                #[cfg(feature = "bedrock")]
                cache: Arc::new(TransportCache::new(self.cache_capacity)),
                #[cfg(feature = "nethernet")]
                http,
                resolver,
                in_flight: Arc::new(tokio::sync::Semaphore::new(
                    self.max_in_flight
                        .unwrap_or(tokio::sync::Semaphore::MAX_PERMITS),
                )),
                queued: Arc::new(tokio::sync::Semaphore::new(
                    self.max_queued
                        .unwrap_or(tokio::sync::Semaphore::MAX_PERMITS),
                )),
                flights: Arc::new(Mutex::new(HashMap::new())),
                merge_in_flight: self.merge_in_flight,
            }),
        })
    }
}

/// Build the singleflight key for a merged query: the effective target
/// endpoint plus every option that changes the probe's outcome.
///
/// The ports come from the parsed address, not from the configured defaults:
/// an explicit `host:port` overrides both defaults, so two queries for the
/// same host on different ports must never share a flight. The timeout is part
/// of the key because it decides the shared probe's deadline.
fn flight_key(address: &ServerAddress, resolved: &ResolvedOptions) -> String {
    use std::fmt::Write as _;

    // One allocation: the host, then every scalar field formatted in place.
    let mut key = String::with_capacity(address.host.len() + 80);
    key.push_str(&address.host);
    // Writing to a `String` cannot fail.
    let _ = write!(
        key,
        "|{}|{}|{:?}|{:?}|{}|{}|{}|{}|{}",
        address.java_port,
        address.bedrock_port,
        address.explicit_port,
        resolved.edition,
        resolved.try_srv,
        resolved.allow_insecure_http,
        resolved.max_response_bytes,
        resolved.max_java_status_bytes,
        resolved.timeout.as_nanos(),
    );
    key
}

/// Build the client's SRV resolver: hickory when the `java` feature is
/// compiled in, and a unit placeholder otherwise.
#[allow(clippy::unnecessary_wraps)]
fn build_srv_resolver() -> Result<SrvResolver, crate::error::Error> {
    #[cfg(feature = "java")]
    {
        hickory_resolver::Resolver::builder_tokio()
            .map_err(|error| crate::error::Error::Dns(error.to_string()))?
            .build()
            .map_err(|error| crate::error::Error::Dns(error.to_string()))
    }
    #[cfg(not(feature = "java"))]
    Ok(())
}

static DEFAULT_CLIENT: OnceLock<std::result::Result<Client, String>> = OnceLock::new();

pub(crate) fn default_client() -> std::result::Result<&'static Client, crate::error::Error> {
    DEFAULT_CLIENT
        .get_or_init(|| Client::builder().build().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|message| crate::error::Error::Http(message.clone()))
}

/// Handle to the client's SRV resolver: the hickory resolver when the
/// `java` feature is compiled in, and a unit placeholder otherwise.
#[cfg(feature = "java")]
pub(crate) type SrvResolver = hickory_resolver::TokioResolver;
#[cfg(not(feature = "java"))]
pub(crate) type SrvResolver = ();

/// One merged in-flight probe shared by concurrent identical callers.
pub(crate) struct Flight {
    /// Broadcasts the shared outcome. A `watch` channel retains the latest
    /// value, so a caller that subscribes after the probe finished still sees
    /// the result instead of waiting for a notification that already fired.
    outcome: tokio::sync::watch::Sender<Option<Arc<Result<Status, Error>>>>,
}

impl Flight {
    fn new() -> Self {
        let (outcome, _receiver) = tokio::sync::watch::channel(None);
        Self { outcome }
    }
}

/// How a caller entered the merged path.
enum Admission {
    /// The flight already existed; wait for its outcome.
    Joined(Arc<Flight>),
    /// This caller created the flight and owns the shared probe.
    Created(Arc<Flight>, tokio::sync::OwnedSemaphorePermit),
}

impl Client {
    /// Run a full query merged with concurrent identical callers.
    ///
    /// The creator of a flight runs the shared probe in a detached task, so
    /// its own cancellation cannot cancel the probe for the others. Every
    /// caller — the creator included — waits for the shared outcome within its
    /// own deadline.
    ///
    /// A merged caller shares the probe that is *currently* running, not a
    /// fresh full timeout window: a late joiner receives the shared result as
    /// soon as it lands, which is always before its own deadline because the
    /// shared probe's absolute deadline was fixed earlier. Failures are shared
    /// too: a waiter never re-probes, so an unreachable server cannot turn one
    /// merged probe into a request storm.
    async fn merged_probe(
        &self,
        key: &str,
        started: Instant,
        deadline: Instant,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
    ) -> Result<Status, Error> {
        let admission = self.admit(key, deadline).await?;
        let flight = match &admission {
            Admission::Joined(flight) | Admission::Created(flight, _) => Arc::clone(flight),
        };

        // Subscribe before reading the outcome: `watch` retains the latest
        // value, so a result that lands between these two steps is still
        // observed instead of being lost.
        let mut outcome = flight.outcome.subscribe();

        if let Admission::Created(entry, permit) = admission {
            // Creator: run the probe in a detached task so this caller's
            // cancellation cannot abort it for the other waiters. The guard
            // cleans the map entry and publishes a failure even if the task
            // panics; the permit bounds how many flights may exist at once.
            let probe_flights = Arc::clone(&self.inner.flights);
            let probe_key = key.to_string();
            let parts = self.parts();
            let address = address.clone();
            let resolved = resolved.clone();
            // `SrvResolver` is `()` without the `java` feature; cloning the
            // unit placeholder is intentional and harmless in that build.
            #[allow(clippy::let_unit_value, clippy::clone_on_copy)]
            let srv_resolver = self.srv_resolver().clone();
            tokio::spawn(async move {
                let _queue_permit = permit;
                let guard = FlightGuard::new(probe_flights, probe_key, Arc::clone(&entry));
                let shared = Arc::new(
                    engine::full_with_deadline(
                        &parts,
                        &address,
                        &resolved,
                        &srv_resolver,
                        started,
                        deadline,
                    )
                    .await,
                );
                guard.complete(shared);
            });
        }

        match wait_for_flight(&mut outcome, deadline).await? {
            Some(shared) => settle(shared.as_ref(), started),
            None => Err(Error::Io(Arc::new(std::io::Error::other(
                "shared query task ended without a result",
            )))),
        }
    }

    /// Join the flight for `key`, creating it when absent.
    ///
    /// Creating a flight first acquires a queue slot, so the number of live
    /// flights (and their detached tasks) is bounded by `max_queued`. The map
    /// is re-checked after waiting for a slot: another caller may have created
    /// the same flight meanwhile, in which case this caller joins it and the
    /// slot is released immediately.
    async fn admit(&self, key: &str, deadline: Instant) -> Result<Admission, Error> {
        if let Some(existing) = self
            .inner
            .flights
            .lock()
            .expect("flights lock poisoned")
            .get(key)
        {
            return Ok(Admission::Joined(Arc::clone(existing)));
        }

        let permit = acquire_queue_slot(&self.inner.queued, deadline).await?;
        let mut flights = self.inner.flights.lock().expect("flights lock poisoned");
        if let Some(existing) = flights.get(key) {
            // Another caller created this flight while we waited for a slot.
            // Dropping `permit` at the end of this block releases the slot.
            return Ok(Admission::Joined(Arc::clone(existing)));
        }
        let entry = Arc::new(Flight::new());
        flights.insert(key.to_string(), Arc::clone(&entry));
        Ok(Admission::Created(entry, permit))
    }
}

/// Acquire one merged-flight slot, waiting no longer than `deadline`.
async fn acquire_queue_slot(
    queued: &Arc<tokio::sync::Semaphore>,
    deadline: Instant,
) -> Result<tokio::sync::OwnedSemaphorePermit, Error> {
    let permit = time::timeout(
        engine::remaining(deadline)?,
        Arc::clone(queued).acquire_owned(),
    )
    .await
    .map_err(|_| Error::Timeout)?;
    permit.map_err(|_| {
        // Unreachable while the client holds the semaphore open, but the
        // closed case must surface as an error, never a panic.
        Error::Io(Arc::new(std::io::Error::other(
            "client queue limiter closed",
        )))
    })
}

/// Cleans up a flight if its shared task exits without publishing a result.
///
/// The normal path calls [`FlightGuard::complete`], which disarms the guard.
/// If the task panics or is aborted first, `Drop` removes the map entry and
/// publishes a failure, so the key cannot stay wedged behind a flight that
/// nobody owns.
struct FlightGuard {
    flights: Arc<Mutex<HashMap<String, Arc<Flight>>>>,
    key: String,
    entry: Arc<Flight>,
    armed: bool,
}

impl FlightGuard {
    fn new(
        flights: Arc<Mutex<HashMap<String, Arc<Flight>>>>,
        key: String,
        entry: Arc<Flight>,
    ) -> Self {
        Self {
            flights,
            key,
            entry,
            armed: true,
        }
    }

    /// Publish the outcome, remove the flight, and disarm the panic path.
    fn complete(mut self, shared: Arc<Result<Status, Error>>) {
        self.armed = false;
        remove_flight_if_current(&self.flights, &self.key, &self.entry);
        self.entry.outcome.send_replace(Some(shared));
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Best effort: a poisoned map is already unusable, and panicking in
        // `drop` during unwinding would abort the process.
        if let Ok(mut flights) = self.flights.lock()
            && flights
                .get(&self.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
        {
            flights.remove(&self.key);
        }
        self.entry
            .outcome
            .send_replace(Some(Arc::new(Err(Error::Io(Arc::new(
                std::io::Error::other("shared query task panicked"),
            ))))));
    }
}

/// Wait for the shared outcome, bounded by this caller's deadline.
///
/// Returns `Ok(None)` when the shared task disappeared without publishing an
/// outcome (runtime shutdown or a panic).
async fn wait_for_flight(
    outcome: &mut tokio::sync::watch::Receiver<Option<Arc<Result<Status, Error>>>>,
    deadline: Instant,
) -> Result<Option<Arc<Result<Status, Error>>>, Error> {
    loop {
        // Re-check the retained value before every wait: this is what makes
        // an outcome that landed before the wait started impossible to miss.
        let shared = { outcome.borrow_and_update().clone() };
        if shared.is_some() {
            return Ok(shared);
        }
        match time::timeout(engine::remaining(deadline)?, outcome.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Ok(None),
            Err(_) => return Err(Error::Timeout),
        }
    }
}

/// Remove `key` from the flight map only while it still maps to `flight`, so a
/// late cleanup can never delete a newer flight with the same key.
fn remove_flight_if_current(
    flights: &Mutex<HashMap<String, Arc<Flight>>>,
    key: &str,
    flight: &Arc<Flight>,
) {
    let mut flights = flights.lock().expect("flights lock poisoned");
    if flights
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, flight))
    {
        flights.remove(key);
    }
}

/// Turn a shared outcome into this caller's own result.
fn settle(shared: &Result<Status, Error>, started: Instant) -> Result<Status, Error> {
    match shared {
        Ok(status) => {
            let mut status = status.clone();
            // Report this caller's own end-to-end latency, including any time
            // spent waiting for the shared probe.
            status.elapsed = started.elapsed();
            Ok(status)
        }
        Err(error) => Err(error.clone()),
    }
}

/// Internal query engine shared by the free functions and [`Client`].
pub(crate) mod engine {
    #[cfg(any(feature = "nethernet", all(feature = "java", feature = "bedrock")))]
    use std::future::Future;
    use std::net::IpAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::net::lookup_host;
    use tokio::time;

    use super::{Parts, ResolvedOptions, SrvResolver};
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
        parts: &Parts,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
        srv_resolver: &SrvResolver,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        full_with_deadline(parts, address, resolved, srv_resolver, started, deadline).await
    }

    /// Run a full query inside an absolute `[started, deadline]` window,
    /// holding one in-flight permit for the whole query.
    ///
    /// Merged queries use this entry so the shared probe runs under the
    /// owner's absolute deadline instead of a fresh timeout, and waiters can
    /// never extend their own deadline by re-probing.
    pub(crate) async fn full_with_deadline(
        parts: &Parts,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
        srv_resolver: &SrvResolver,
        started: Instant,
        deadline: Instant,
    ) -> Result<Status> {
        // One permit per logical query. The nested transport layers
        // (`full -> bedrock -> raknet`) deliberately do not acquire again, so
        // `max_in_flight(1)` cannot deadlock a query against itself.
        let _permit = acquire_in_flight(parts, deadline).await?;
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
            None => auto_probe(parts, address, resolved, deadline, srv_resolver).await,
        };
        finish(started, result)
    }

    /// Acquire one in-flight permit, waiting no longer than `deadline`.
    async fn acquire_in_flight(
        parts: &Parts,
        deadline: Instant,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, Error> {
        let permit = time::timeout(
            remaining(deadline)?,
            Arc::clone(&parts.in_flight).acquire_owned(),
        )
        .await
        .map_err(|_| Error::Timeout)?;
        permit.map_err(|_| {
            // Unreachable while `Parts` holds the semaphore open, but the
            // closed case must still surface as an error, never a panic.
            Error::Io(Arc::new(std::io::Error::other(
                "client concurrency limiter closed",
            )))
        })
    }

    /// Run a Java-only query.
    #[cfg(feature = "java")]
    pub(crate) async fn java(
        parts: &Parts,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
        srv_resolver: &hickory_resolver::TokioResolver,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        let _permit = acquire_in_flight(parts, deadline).await?;
        finish(
            started,
            java::run(address, resolved, deadline, srv_resolver).await,
        )
    }

    /// Run a Bedrock-only query with automatic transport selection.
    #[cfg(feature = "bedrock")]
    pub(crate) async fn bedrock(
        parts: &Parts,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        let _permit = acquire_in_flight(parts, deadline).await?;
        finish(
            started,
            bedrock::run(parts, address, resolved, deadline).await,
        )
    }

    /// Run a forced `RakNet` query; the transport error surfaces directly.
    #[cfg(feature = "bedrock")]
    pub(crate) async fn raknet(
        parts: &Parts,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        let _permit = acquire_in_flight(parts, deadline).await?;
        let ips = resolve_probe_ips(&address.host, deadline).await?;
        let result = bedrock::raknet::probe(address, deadline, &ips, parts).await;
        finish(started, result.map_err(|attempt| *attempt.error))
    }

    /// Run a forced `NetherNet` query; the transport error surfaces directly.
    #[cfg(feature = "nethernet")]
    pub(crate) async fn nethernet(
        parts: &Parts,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
    ) -> Result<Status> {
        let started = Instant::now();
        let deadline = started
            .checked_add(resolved.timeout)
            .ok_or(Error::InvalidInput("query timeout is too large"))?;
        let _permit = acquire_in_flight(parts, deadline).await?;
        let result =
            crate::bedrock::nethernet::probe(address, deadline, parts, resolved, true).await;
        finish(started, result.map_err(|attempt| *attempt.error))
    }

    /// Probe every compiled edition when no edition restriction is set.
    async fn auto_probe(
        parts: &Parts,
        address: &ServerAddress,
        resolved: &ResolvedOptions,
        deadline: Instant,
        srv_resolver: &SrvResolver,
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
            #[allow(clippy::ignored_unit_patterns)]
            let () = srv_resolver;
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
        parts: &Parts,
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

#[cfg(all(test, feature = "bedrock"))]
mod concurrency_tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    const MOCK_MOTD_TEXT: &str =
        "MCPE;RakNet Mock;589;1.21.100;3;20;12345;Line2;Survival;1;19132;19133;";

    /// A local UDP server that answers `RakNet` unconnected pings.
    struct MockServer {
        address: SocketAddr,
        /// Pings received.
        pings: Arc<AtomicUsize>,
        /// Maximum number of pings that were outstanding at the same time.
        max_active: Arc<AtomicUsize>,
    }

    #[tokio::test]
    async fn merge_in_flight_shares_one_probe_for_concurrent_identical_queries() {
        // Two concurrent queries with the same target/config share a single
        // probe when merge_in_flight is enabled. A RakNet mock answers, so
        // both callers get a successful status.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::ZERO, true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());

        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");

        let first = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move { client.query(&address).await })
        };
        let second = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move { client.query(&address).await })
        };

        let a = first
            .await
            .expect("task should finish")
            .expect("first query should answer");
        let b = second
            .await
            .expect("task should finish")
            .expect("second query should answer");
        assert_eq!(a.motd, "RakNet Mock");
        assert_eq!(b.motd, "RakNet Mock");
        assert_eq!(a.transport(), Transport::RakNetUdp);
        assert_eq!(b.transport(), Transport::RakNetUdp);
    }

    #[tokio::test]
    async fn max_in_flight_bounds_concurrent_probes() {
        // A tight limit must cap how many logical queries run at once. The
        // mock answers after a delay, so overlapping queries would show up as
        // overlapping pings. Going through `query_with` also proves the
        // `full -> bedrock -> raknet` chain takes exactly one permit: a
        // nested acquisition would deadlock `max_in_flight(1)`.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::from_millis(5), true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .max_in_flight(1)
            .timeout(Duration::from_secs(10))
            .build()
            .expect("test client should build");

        let mut tasks = Vec::new();
        for _ in 0..100 {
            let client = client.clone();
            let address = address.clone();
            tasks.push(tokio::spawn(async move {
                client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await
            }));
        }
        for task in tasks {
            let status = task
                .await
                .expect("task should finish")
                .expect("queued query should answer");
            assert_eq!(status.motd, "RakNet Mock");
        }

        assert_eq!(
            mock.pings.load(Ordering::SeqCst),
            100,
            "every query should probe exactly once"
        );
        assert_eq!(
            mock.max_active.load(Ordering::SeqCst),
            1,
            "max_in_flight(1) must serialize logical queries"
        );
    }

    #[tokio::test]
    async fn same_host_different_ports_are_never_merged() {
        // The review's scenario: two endpoints on the same host with different
        // ports. A flight key built from the configured defaults instead of
        // the parsed address would merge them and return the wrong status.
        let first = spawn_mock(
            "MCPE;First Port;589;1.21.100;1;10;1;line;Survival;1;19132;19133;",
            Duration::from_millis(5),
            true,
        )
        .await;
        let second = spawn_mock(
            "MCPE;Second Port;589;1.21.100;2;10;2;line;Survival;1;19132;19133;",
            Duration::from_millis(5),
            true,
        )
        .await;
        assert_eq!(
            first.address.ip(),
            second.address.ip(),
            "both mocks must share a host"
        );

        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_secs(5))
            .build()
            .expect("test client should build");
        let first_address = format!("{}:{}", first.address.ip(), first.address.port());
        let second_address = format!("{}:{}", second.address.ip(), second.address.port());

        let (a, b) = tokio::join!(
            client.query_with(
                first_address,
                QueryOptions::default().edition(Edition::Bedrock)
            ),
            client.query_with(
                second_address,
                QueryOptions::default().edition(Edition::Bedrock)
            ),
        );

        assert_eq!(a.expect("first port should answer").motd, "First Port");
        assert_eq!(b.expect("second port should answer").motd, "Second Port");
    }

    #[tokio::test]
    async fn shared_failure_is_not_repeated_by_waiters() {
        // The mock never answers, so the shared probe fails at its deadline.
        // Waiters must receive that shared failure instead of each launching a
        // fresh probe (a request storm) or extending their own deadline.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::ZERO, false).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_millis(600))
            .build()
            .expect("test client should build");

        let barrier = Arc::new(tokio::sync::Barrier::new(100));
        let mut tasks = Vec::new();
        for _ in 0..100 {
            let client = client.clone();
            let address = address.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await
            }));
        }

        let started = Instant::now();
        for task in tasks {
            let error = task
                .await
                .expect("task should finish")
                .expect_err("a silent server must fail");
            assert!(
                matches!(
                    error,
                    Error::NoCompatibleBedrockServer { .. } | Error::Timeout
                ),
                "unexpected shared failure: {error:?}"
            );
        }
        assert_eq!(
            mock.pings.load(Ordering::SeqCst),
            1,
            "waiters must not re-probe a shared failure"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waiters must stop at the shared deadline, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn cancelling_one_caller_does_not_affect_the_others() {
        // The owner's caller is cancelled while the shared probe is still in
        // flight; the detached probe must still answer the remaining waiter.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::from_millis(50), true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");

        let owner = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move {
                client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await
            })
        };
        // Let the owner claim the flight and spawn the shared probe.
        tokio::task::yield_now().await;
        let waiter = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move {
                client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await
            })
        };
        tokio::task::yield_now().await;

        owner.abort();
        let status = waiter
            .await
            .expect("waiter should finish")
            .expect("the waiter should still receive the shared result");
        assert_eq!(status.motd, "RakNet Mock");
    }

    #[tokio::test]
    async fn cancelled_callers_leave_no_stale_flight() {
        // Every caller cancels while the shared probe runs. The detached probe
        // still finishes, publishes its outcome, and removes its map entry; a
        // later query must start a fresh probe and answer.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::from_millis(50), true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let client = client.clone();
            let address = address.clone();
            tasks.push(tokio::spawn(async move {
                client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await
            }));
        }
        tokio::task::yield_now().await;
        for task in &tasks {
            task.abort();
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let empty = client
                .inner
                .flights
                .lock()
                .expect("flights lock poisoned")
                .is_empty();
            if empty {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the detached probe never cleaned up its flight"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let status = client
            .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
            .await
            .expect("a fresh query should answer");
        assert_eq!(status.motd, "RakNet Mock");
        assert_eq!(
            mock.pings.load(Ordering::SeqCst),
            2,
            "one shared probe plus one fresh query"
        );
    }

    #[tokio::test]
    async fn immediately_completing_shared_probes_are_never_missed() {
        // The mock answers instantly, so a shared probe can finish before a
        // waiter starts waiting. `watch` retains the outcome, so no waiter may
        // time out on a notification that already fired.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::ZERO, true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client should build");

        let mut tasks = Vec::new();
        for _ in 0..64 {
            let client = client.clone();
            let address = address.clone();
            tasks.push(tokio::spawn(async move {
                client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await
            }));
        }
        for task in tasks {
            let status = task
                .await
                .expect("task should finish")
                .expect("an instantly answering server must never time out");
            assert_eq!(status.motd, "RakNet Mock");
        }
    }

    #[test]
    fn stale_cleanup_keeps_a_newer_flight() {
        let old = Arc::new(Flight::new());
        let new = Arc::new(Flight::new());
        let flights = Mutex::new(HashMap::new());
        flights
            .lock()
            .expect("flights lock poisoned")
            .insert("key".to_string(), Arc::clone(&old));
        // A newer flight took over the same key.
        flights
            .lock()
            .expect("flights lock poisoned")
            .insert("key".to_string(), Arc::clone(&new));

        remove_flight_if_current(&flights, "key", &old);
        assert!(
            flights
                .lock()
                .expect("flights lock poisoned")
                .contains_key("key"),
            "a stale owner must not remove a newer flight"
        );

        remove_flight_if_current(&flights, "key", &new);
        assert!(flights.lock().expect("flights lock poisoned").is_empty());
    }

    #[test]
    fn flight_key_uses_the_actual_target_ports() {
        let resolved = QueryOptions::default().resolve_over(&QueryOptions::default());
        let first = crate::address::parse_server_address("127.0.0.1:19132", 25_565, 19_132)
            .expect("address should parse");
        let second = crate::address::parse_server_address("127.0.0.1:19133", 25_565, 19_132)
            .expect("address should parse");
        assert_ne!(
            flight_key(&first, &resolved),
            flight_key(&second, &resolved),
            "different target ports must never share a flight"
        );

        // With identical effective ports, only `explicit_port` distinguishes
        // "use the default" from "an explicit port was given"; SRV lookup
        // depends on exactly that distinction.
        let resolved = QueryOptions::default()
            .bedrock_port(25_565)
            .resolve_over(&QueryOptions::default());
        let implicit = crate::address::parse_server_address("127.0.0.1", 25_565, 25_565)
            .expect("address should parse");
        let explicit = crate::address::parse_server_address("127.0.0.1:25565", 25_565, 25_565)
            .expect("address should parse");
        assert_eq!(implicit.java_port, explicit.java_port);
        assert_eq!(implicit.bedrock_port, explicit.bedrock_port);
        assert_ne!(
            flight_key(&implicit, &resolved),
            flight_key(&explicit, &resolved),
            "the explicit-port flag changes SRV behavior and belongs in the key"
        );
    }

    #[test]
    fn max_in_flight_rejects_zero() {
        let error = Client::builder()
            .max_in_flight(0)
            .build()
            .err()
            .expect("zero must be rejected");

        assert!(matches!(error, Error::InvalidInput(_)));
    }

    #[test]
    fn max_in_flight_rejects_values_above_the_semaphore_limit() {
        let error = Client::builder()
            .max_in_flight(tokio::sync::Semaphore::MAX_PERMITS + 1)
            .build()
            .err()
            .expect("over-limit must be rejected");

        assert!(matches!(error, Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn callers_with_different_timeouts_never_extend_each_other() {
        // Different timeouts produce different flight keys, so each caller
        // probes under its own deadline. A shared flight would make the long
        // caller return at the short caller's deadline (or vice versa).
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::ZERO, false).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_millis(700))
            .build()
            .expect("test client should build");

        let short = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                let result = client
                    .query_with(
                        address,
                        QueryOptions::default()
                            .edition(Edition::Bedrock)
                            .timeout(Duration::from_millis(150)),
                    )
                    .await;
                (started.elapsed(), result)
            })
        };
        let long = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                let result = client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await;
                (started.elapsed(), result)
            })
        };

        let (short_elapsed, short_result) = short.await.expect("task should finish");
        let (long_elapsed, long_result) = long.await.expect("task should finish");
        assert!(short_result.is_err(), "the short caller must time out");
        assert!(long_result.is_err(), "the long caller must time out");
        assert!(
            short_elapsed < Duration::from_millis(600),
            "the short caller must stop at its own deadline: {short_elapsed:?}"
        );
        assert!(
            long_elapsed >= Duration::from_millis(400),
            "the long caller must not inherit the short deadline: {long_elapsed:?}"
        );
        assert!(
            long_elapsed < Duration::from_secs(3),
            "the long caller must still stop at its own deadline: {long_elapsed:?}"
        );
    }

    #[tokio::test]
    async fn flight_map_drains_after_a_burst() {
        // Every merged query must clean up its entry, whether it succeeded,
        // failed, or had no waiters.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::ZERO, true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");

        for _ in 0..50 {
            client
                .query_with(
                    address.clone(),
                    QueryOptions::default().edition(Edition::Bedrock),
                )
                .await
                .expect("mock server should answer");
        }

        assert!(
            client
                .inner
                .flights
                .lock()
                .expect("flights lock poisoned")
                .is_empty(),
            "completed flights must not accumulate in the map"
        );
    }

    #[tokio::test]
    async fn late_joiners_share_the_current_probe() {
        // The first caller starts the shared probe; the second joins 60 ms
        // later with the same options. It must receive the shared result
        // instead of starting a second probe, and stay inside its deadline.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::from_millis(120), true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .timeout(Duration::from_secs(2))
            .build()
            .expect("test client should build");

        let first = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move {
                client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(60)).await;
        let second = {
            let client = client.clone();
            let address = address.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                let result = client
                    .query_with(address, QueryOptions::default().edition(Edition::Bedrock))
                    .await;
                (started.elapsed(), result)
            })
        };

        let a = first
            .await
            .expect("task should finish")
            .expect("first query should answer");
        let (second_elapsed, b) = second.await.expect("task should finish");
        assert_eq!(a.motd, "RakNet Mock");
        assert_eq!(
            b.expect("the late joiner should answer").motd,
            "RakNet Mock"
        );
        assert!(
            second_elapsed < Duration::from_secs(1),
            "the late joiner must not restart the probe: {second_elapsed:?}"
        );
        assert_eq!(
            mock.pings.load(Ordering::SeqCst),
            1,
            "a late joiner must not add a probe"
        );
    }

    #[tokio::test]
    async fn max_queued_bounds_live_flights() {
        // Ten distinct keys targeting the same server. With one queue slot
        // only one flight may exist at a time; the others wait for the slot
        // and run afterwards instead of accumulating detached tasks.
        let mock = spawn_mock(MOCK_MOTD_TEXT, Duration::from_millis(20), true).await;
        let address = format!("{}:{}", mock.address.ip(), mock.address.port());
        let client = Client::builder()
            .merge_in_flight(true)
            .max_queued(1)
            .timeout(Duration::from_secs(5))
            .build()
            .expect("test client should build");

        let max_seen = Arc::new(AtomicUsize::new(0));
        let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let watcher = {
            let flights = Arc::clone(&client.inner.flights);
            let max_seen = Arc::clone(&max_seen);
            let watching = Arc::clone(&watching);
            tokio::spawn(async move {
                while watching.load(Ordering::SeqCst) {
                    let live = flights.lock().expect("flights lock poisoned").len();
                    max_seen.fetch_max(live, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
        };

        let mut tasks = Vec::new();
        for index in 0..10_u32 {
            let client = client.clone();
            let address = address.clone();
            // `max_response_bytes` is part of the flight key, so every caller
            // asks for a distinct flight against the same endpoint.
            let options = QueryOptions::default()
                .edition(Edition::Bedrock)
                .max_response_bytes(1024 + usize::try_from(index).expect("index fits"));
            tasks.push(tokio::spawn(async move {
                client.query_with(address, options).await
            }));
        }
        for task in tasks {
            let status = task
                .await
                .expect("task should finish")
                .expect("queued query should answer");
            assert_eq!(status.motd, "RakNet Mock");
        }

        watching.store(false, Ordering::SeqCst);
        watcher.await.expect("watcher should finish");
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "max_queued(1) must keep at most one live flight"
        );
    }

    #[test]
    fn max_queued_rejects_zero() {
        let error = Client::builder()
            .max_queued(0)
            .build()
            .err()
            .expect("zero must be rejected");

        assert!(matches!(error, Error::InvalidInput(_)));
    }

    #[test]
    fn panicking_shared_task_cleans_up_its_flight() {
        // The guard is what protects the map when the detached probe task
        // unwinds instead of publishing a result.
        let flights = Arc::new(Mutex::new(HashMap::new()));
        let entry = Arc::new(Flight::new());
        flights
            .lock()
            .expect("flights lock poisoned")
            .insert("panic-key".to_string(), Arc::clone(&entry));
        let mut outcome = entry.outcome.subscribe();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let flights = Arc::clone(&flights);
            let entry = Arc::clone(&entry);
            move || {
                let _guard = FlightGuard::new(flights, "panic-key".to_string(), entry);
                panic!("simulated shared-probe panic");
            }
        }));
        assert!(result.is_err(), "the closure should have panicked");

        assert!(
            flights.lock().expect("flights lock poisoned").is_empty(),
            "a panicking shared task must remove its flight"
        );
        let published = outcome.borrow_and_update().clone();
        let published = published.expect("a panic must publish an error outcome");
        assert!(
            matches!(published.as_ref(), Err(Error::Io(_))),
            "the published outcome should be an I/O error"
        );
    }

    #[cfg(feature = "nethernet")]
    #[tokio::test]
    async fn insecure_http_cache_hit_respects_a_stricter_policy() {
        let client = Client::builder()
            .allow_insecure_http(true)
            .timeout(Duration::from_secs(3))
            .build()
            .expect("test client should build");

        // Plant a cached plain-HTTP transport for a closed endpoint, then
        // query the same client with insecure HTTP explicitly disabled.
        // The cached HTTP hit must NOT be used: the probe must fail rather
        // than silently returning a plain-HTTP answer.
        client
            .inner
            .cache
            .store("127.0.0.1", 1, Transport::NetherNetHttp);
        let denied = QueryOptions::default().allow_insecure_http(false);
        let error = client
            .query_with("127.0.0.1:1", denied)
            .await
            .expect_err("a stricter policy must reject the cached HTTP hit");

        let attempts = match &error {
            Error::NoCompatibleBedrockServer { attempts, .. }
            | Error::NoCompatibleServer { attempts, .. } => attempts,
            other => panic!("expected an aggregate error, got {other:?}"),
        };
        // The failed race contains no successful plain-HTTP result: with the
        // policy disabled, no attempt may use the cached insecure transport.
        assert!(
            !attempts
                .iter()
                .any(|attempt| attempt.transport == Transport::NetherNetHttp),
            "cached plain-HTTP hit bypassed the policy: {attempts:?}"
        );
    }

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

    /// Spawn a UDP mock that answers every unconnected ping with `text` after
    /// `delay`, or never answers when `reply` is false. It counts received
    /// pings and the maximum number that were outstanding at the same time.
    async fn spawn_mock(text: &'static str, delay: Duration, reply: bool) -> MockServer {
        use std::net::Ipv4Addr;
        use tokio::net::UdpSocket;

        let socket = Arc::new(
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("mock socket should bind"),
        );
        let address = socket.local_addr().expect("mock address should exist");
        let pings = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));

        let reader = Arc::clone(&socket);
        let ping_counter = Arc::clone(&pings);
        let max_counter = Arc::clone(&max_active);
        let active_counter = Arc::clone(&active);
        tokio::spawn(async move {
            let mut buffer = [0_u8; 64];
            loop {
                let Ok((len, peer)) = reader.recv_from(&mut buffer).await else {
                    return;
                };
                if len != 33 {
                    continue;
                }
                ping_counter.fetch_add(1, Ordering::SeqCst);
                let outstanding = active_counter.fetch_add(1, Ordering::SeqCst) + 1;
                max_counter.fetch_max(outstanding, Ordering::SeqCst);

                let socket = Arc::clone(&reader);
                let active = Arc::clone(&active_counter);
                let ping_time: [u8; 8] = buffer[1..9].try_into().expect("8 bytes");
                tokio::spawn(async move {
                    if reply {
                        tokio::time::sleep(delay).await;
                        // Release the slot before the pong can reach the
                        // client, so a serialized client never looks
                        // overlapping just because of task scheduling.
                        active.fetch_sub(1, Ordering::SeqCst);
                        let pong = build_pong(ping_time, text);
                        let _ = socket.send_to(&pong, peer).await;
                    } else {
                        tokio::time::sleep(delay).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });

        MockServer {
            address,
            pings,
            max_active,
        }
    }
}
