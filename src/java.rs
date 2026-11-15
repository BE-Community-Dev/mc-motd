//! Java Edition status queries.
//!
//! [`query`] resolves `_minecraft._tcp` SRV records when the address has no
//! explicit port, then probes the Java status endpoint over TCP. For custom
//! timeouts and options use [`query_with`] or [`crate::Client`]. The pure
//! protocol codec lives in [`crate::protocol::java`].

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time;

use crate::address::{self, ServerAddress};
use crate::client::{QueryOptions, ResolvedOptions};
use crate::error::{Attempt, Error, Result};
use crate::protocol::java as codec;
use crate::status::{Details, JavaDetails, Players, Status, Transport, Version};

/// Initial capacity for a Java status packet buffer.
///
/// The length prefix is a remote claim; a server that declares a large packet
/// and then stalls must not force that much memory to be allocated up front.
/// The buffer starts small and grows as bytes actually arrive.
const PACKET_RESERVE_HINT: usize = 8 * 1024;

/// Java Edition SRV lookup target.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

/// Query a Java Edition server address.
///
/// The address may be `host`, `host:port`, or `[ipv6]:port`. Without an
/// explicit port, `_minecraft._tcp` SRV records are resolved first and the
/// direct Java default port is used as fallback.
///
/// # Errors
///
/// Returns [`Error`] if the address is invalid, DNS lookup fails, the network
/// exchange fails or exceeds the deadline, or no Java server answers.
pub async fn query(address: impl AsRef<str>) -> Result<Status> {
    crate::client::default_client()?.java(address).await
}

/// Query a Java Edition server address with explicit options.
///
/// The `edition` field of `options` is ignored here; this function always
/// probes Java.
///
/// # Errors
///
/// Returns [`Error`] under the same conditions as [`query`].
pub async fn query_with(address: impl AsRef<str>, options: QueryOptions) -> Result<Status> {
    crate::client::default_client()?
        .java_with(address, options)
        .await
}

/// Resolve `_minecraft._tcp` SRV records for a host.
///
/// Records are ordered by priority; within one priority the standard SRV
/// weighted-random selection is applied. IP addresses and IPv6 literals
/// return an empty list because SRV records only apply to DNS names.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the host is invalid, [`Error::Timeout`]
/// if the lookup exceeds `timeout`, or [`Error::Dns`] if resolver setup or
/// lookup fails.
pub async fn resolve_srv(host: impl AsRef<str>, timeout: Duration) -> Result<Vec<SrvTarget>> {
    let resolver = crate::client::default_client()?.srv_resolver().clone();
    resolve_srv_with(&resolver, host, timeout).await
}

/// Typed outcome of an SRV lookup.
///
/// Control flow never inspects error message text: an authoritative
/// "service unavailable" answer is a distinct outcome, not a DNS failure.
pub(crate) enum SrvLookup {
    /// Usable targets, already ordered per RFC 2782.
    Targets(Vec<SrvTarget>),
    /// The host has no SRV records, or SRV does not apply to it.
    NoRecords,
    /// The records authoritatively mark the service as unavailable (`.` target
    /// or port 0).
    Unavailable,
}

/// Resolve SRV records with a caller-supplied resolver so client queries
/// reuse one resolver per client instead of rebuilding it per query.
pub(crate) async fn resolve_srv_with(
    resolver: &hickory_resolver::TokioResolver,
    host: impl AsRef<str>,
    timeout: Duration,
) -> Result<Vec<SrvTarget>> {
    match lookup_srv(resolver, host.as_ref(), timeout).await? {
        SrvLookup::Targets(targets) => Ok(targets),
        SrvLookup::NoRecords => Ok(Vec::new()),
        SrvLookup::Unavailable => Err(Error::Dns(
            "SRV records mark the service as unavailable".into(),
        )),
    }
}

/// Look up SRV records and classify the outcome.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] if the host is invalid, [`Error::Timeout`]
/// if the lookup exceeds `timeout`, and [`Error::Dns`] if the resolver
/// reports a lookup failure.
pub(crate) async fn lookup_srv(
    resolver: &hickory_resolver::TokioResolver,
    host: &str,
    timeout: Duration,
) -> Result<SrvLookup> {
    let host = address::normalize_server_host(host)?;
    if !address::can_query_srv(host.as_ref()) {
        return Ok(SrvLookup::NoRecords);
    }

    let name = address::minecraft_srv_name(host.as_ref());
    let lookup = time::timeout(timeout, resolver.srv_lookup(name))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|error| Error::Dns(error.to_string()))?;

    let records = lookup
        .answers()
        .iter()
        .filter_map(|record| match &record.data {
            hickory_resolver::proto::rr::RData::SRV(srv) => Some(SrvTarget {
                host: srv.target.to_utf8().trim_end_matches('.').to_string(),
                port: srv.port,
                priority: srv.priority,
                weight: srv.weight,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Ok(SrvLookup::NoRecords);
    }
    // RFC 2782: a "." target or port 0 means the service is decidedly
    // unavailable — that is an authoritative answer, not a fallback signal.
    if records
        .iter()
        .all(|target| target.host.is_empty() || target.port == 0)
    {
        return Ok(SrvLookup::Unavailable);
    }
    let usable = records
        .into_iter()
        .filter(|target| !target.host.is_empty() && target.port != 0)
        .collect::<Vec<_>>();
    Ok(SrvLookup::Targets(weighted_srv_order(usable)))
}

/// Order SRV records by priority with RFC 2782 weighted-random selection
/// inside each priority class.
///
/// The random source is seeded from the standard library's OS-entropy hasher
/// ([`std::collections::hash_map::RandomState`]); tests use
/// [`weighted_srv_order_with`] with a fixed seed so distribution assertions are
/// deterministic.
fn weighted_srv_order(records: Vec<SrvTarget>) -> Vec<SrvTarget> {
    let mut seed = random_seed();
    let mut random = move || {
        // xorshift64: tiny dependency-free PRNG, good enough for ordering.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    weighted_srv_order_with(records, &mut random)
}

/// Draw a fresh 64-bit seed for the ordering PRNG.
///
/// `RandomState` is seeded by the standard library from OS entropy and its
/// per-call keys differ even within one clock tick, so concurrent lookups do
/// not share an ordering sequence. The atomic counter adds an independent
/// input without taking a lock.
fn random_seed() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    // xorshift must not be seeded with zero.
    hasher.finish() | 1
}

/// Order SRV records with a caller-supplied random source.
fn weighted_srv_order_with(
    mut records: Vec<SrvTarget>,
    random: &mut impl FnMut() -> u64,
) -> Vec<SrvTarget> {
    records.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.host.cmp(&right.host))
            .then_with(|| left.port.cmp(&right.port))
    });

    let mut ordered = Vec::with_capacity(records.len());
    while !records.is_empty() {
        let priority = records[0].priority;
        let group_end = records
            .iter()
            .take_while(|target| target.priority == priority)
            .count();
        let mut group: Vec<SrvTarget> = records.drain(0..group_end).collect();
        while !group.is_empty() {
            // RFC 2782: zero-weight records are placed at the front of the
            // list. Their relative order is arbitrary, so shuffle them: an
            // all-zero group then still selects uniformly instead of always
            // returning the first record.
            group.sort_by_key(|target| target.weight != 0);
            let zero_count = group.iter().take_while(|target| target.weight == 0).count();
            if zero_count > 1 {
                for index in (1..zero_count).rev() {
                    let upper = u64::try_from(index + 1).unwrap_or(u64::MAX);
                    let swap = usize::try_from(random() % upper).unwrap_or(0);
                    group.swap(index, swap);
                }
            }

            let total: u64 = group.iter().map(|target| u64::from(target.weight)).sum();
            // RFC 2782 samples the inclusive range [0, sum], so a zero-weight
            // record is selected when the random point is exactly zero.
            let mut point = random() % total.saturating_add(1);
            let mut picked = group.len() - 1;
            for (position, target) in group.iter().enumerate() {
                let weight = u64::from(target.weight);
                if point <= weight {
                    picked = position;
                    break;
                }
                point -= weight;
            }
            ordered.push(group.remove(picked));
        }
    }
    ordered
}

/// Whether the SRV lookup should run for this address.
///
/// SRV applies only when the caller let the library choose the port (no
/// explicit port), the option is enabled, and the host is a DNS name.
fn srv_applicable(address: &ServerAddress, resolved: &ResolvedOptions) -> bool {
    resolved.try_srv && address.explicit_port.is_none() && address::can_query_srv(&address.host)
}

/// Full Java path used by the engine: SRV records first, then direct.
pub(crate) async fn run(
    address: &ServerAddress,
    resolved: &ResolvedOptions,
    deadline: Instant,
    srv_resolver: &hickory_resolver::TokioResolver,
) -> Result<Status> {
    let mut prefix_attempts: Vec<Attempt> = Vec::new();

    if srv_applicable(address, resolved) {
        // Cap the SRV budget so a slow SRV path cannot starve direct probes.
        let srv_budget =
            crate::client::engine::remaining(deadline)?.min(crate::client::SRV_LOOKUP_BUDGET);
        match lookup_srv(srv_resolver, &address.host, srv_budget).await {
            Ok(SrvLookup::Targets(targets)) => {
                return probe_srv_sequence(address, &targets, resolved, deadline)
                    .await
                    .map_err(|attempts| Error::NoCompatibleJavaServer {
                        host: address.host.clone(),
                        java_port: address.java_port,
                        attempts,
                    });
            }
            Ok(SrvLookup::NoRecords) => prefix_attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: address.host.clone(),
                error: Box::new(Error::Dns("no SRV records found".to_string())),
            }),
            // SRV records pointing at "." are an authoritative "service
            // unavailable": do not silently fall back to direct probing.
            Ok(SrvLookup::Unavailable) => {
                prefix_attempts.push(Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(Error::Dns(
                        "SRV records mark the service as unavailable".to_string(),
                    )),
                });
                return Err(Error::NoCompatibleJavaServer {
                    host: address.host.clone(),
                    java_port: address.java_port,
                    attempts: prefix_attempts,
                });
            }
            Err(error) => prefix_attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: address.host.clone(),
                error: Box::new(error),
            }),
        }
    }

    let ips = match crate::client::engine::resolve_probe_ips(&address.host, deadline).await {
        Ok(ips) => ips,
        Err(error) => {
            // Keep the SRV diagnostics collected above: a direct-resolution
            // failure must not erase what the SRV phase already learned.
            let mut all = std::mem::take(&mut prefix_attempts);
            all.push(Attempt {
                transport: Transport::JavaTcp,
                target: address.host.clone(),
                error: Box::new(error),
            });
            return Err(Error::NoCompatibleJavaServer {
                host: address.host.clone(),
                java_port: address.java_port,
                attempts: all,
            });
        }
    };

    probe_direct(address, &ips, resolved, deadline)
        .await
        .map_err(|mut attempts| {
            let mut all = std::mem::take(&mut prefix_attempts);
            all.append(&mut attempts);
            Error::NoCompatibleJavaServer {
                host: address.host.clone(),
                java_port: address.java_port,
                attempts: all,
            }
        })
}

/// Probe SRV targets in order, then fall back to the direct address.
pub(crate) async fn probe_srv_sequence(
    address: &ServerAddress,
    targets: &[SrvTarget],
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Vec<Attempt>> {
    let mut attempts = Vec::new();
    // Fair-share the remaining deadline across the SRV targets *and* the
    // direct fallback, which runs after them. Without reserving a share, a
    // silent last target consumes the whole deadline and the documented
    // fallback never gets a chance to run.
    let mut stages_left = targets.len() + 1;
    for target in targets {
        let remaining = crate::client::engine::remaining(deadline).unwrap_or_default();
        let target_budget = remaining / u32::try_from(stages_left).unwrap_or(u32::MAX);
        let target_deadline = Instant::now()
            .checked_add(target_budget)
            .unwrap_or(deadline);
        stages_left = stages_left.saturating_sub(1);

        match probe_target(address, target, resolved, target_deadline).await {
            Ok(status) => return Ok(status),
            Err(error) => attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: format!("{}:{}", target.host, target.port),
                error: Box::new(error),
            }),
        }
    }
    // Direct fallback: an empty address list makes probe_direct resolve
    // the input host itself.
    match probe_direct(address, &[], resolved, deadline).await {
        Ok(status) => Ok(status),
        Err(mut direct_attempts) => {
            attempts.append(&mut direct_attempts);
            Err(attempts)
        }
    }
}

/// Probe one SRV target, trying every resolved address in order.
///
/// The TCP connection goes to the SRV target, but the handshake names the host
/// the caller originally asked for: virtual-host proxies route on that name,
/// and vanilla Server List Ping sends the pre-SRV hostname. `Status.address`
/// keeps the requested address and the SRV record lands in
/// [`JavaDetails::srv`].
async fn probe_target(
    requested: &ServerAddress,
    target: &SrvTarget,
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Error> {
    let ips = crate::client::engine::resolve_probe_ips(&target.host, deadline).await?;
    let mut last = None;
    // The target deadline is already a fair slice of the whole query; split
    // it across the target's own addresses.
    let mut attempts_left = ips.len();
    for ip in ips {
        let remaining = crate::client::engine::remaining(deadline)?;
        let attempt_deadline = Instant::now()
            .checked_add(remaining / u32::try_from(attempts_left).unwrap_or(u32::MAX))
            .unwrap_or(deadline);
        attempts_left = attempts_left.saturating_sub(1);

        let socket = SocketAddr::new(ip, target.port);
        match query_socket(
            socket,
            // Handshake host = the requested name, not the SRV target; the
            // handshake port is the port the connection actually uses.
            &requested.host,
            target.port,
            requested,
            resolved,
            attempt_deadline,
        )
        .await
        {
            Ok(status) => return Ok(mark_srv(status, target)),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or(Error::ResolveFailed))
}

/// Stamp the SRV record onto a Java status payload.
fn mark_srv(mut status: Status, target: &SrvTarget) -> Status {
    #[allow(irrefutable_let_patterns)]
    if let Status {
        details: Details::Java(details),
        ..
    } = &mut status
    {
        details.srv = Some(target.clone());
    }
    status
}

/// Probe the direct Java port, trying every resolved address in order until
/// one answers. With an empty `ips` slice the input host is resolved first.
/// The whole sequence is deadline-bounded end to end.
pub(crate) async fn probe_direct(
    address: &ServerAddress,
    ips: &[IpAddr],
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Vec<Attempt>> {
    let mut attempts = Vec::new();
    let resolved_ips;
    let candidate_ips: &[IpAddr] = if ips.is_empty() {
        resolved_ips = crate::client::engine::resolve_probe_ips(&address.host, deadline)
            .await
            .map_err(|error| {
                vec![Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(error),
                }]
            })?;
        &resolved_ips
    } else {
        ips
    };

    // Fair-share the shared deadline across the addresses so a silently
    // dropping first address gets its slice and the remaining candidates
    // split whatever is left.
    let mut attempts_left = candidate_ips.len();
    for ip in candidate_ips {
        let remaining = crate::client::engine::remaining(deadline).unwrap_or_default();
        let attempt_budget = remaining / u32::try_from(attempts_left).unwrap_or(u32::MAX);
        let attempt_deadline = Instant::now()
            .checked_add(attempt_budget)
            .unwrap_or(deadline);
        attempts_left = attempts_left.saturating_sub(1);

        let socket = SocketAddr::new(*ip, address.java_port);
        match query_socket(
            socket,
            &address.host,
            address.java_port,
            address,
            resolved,
            attempt_deadline,
        )
        .await
        {
            Ok(status) => return Ok(status),
            Err(error) => attempts.push(Attempt {
                transport: Transport::JavaTcp,
                target: format!("{}:{}", address.host, address.java_port),
                error: Box::new(error),
            }),
        }
    }
    Err(attempts)
}

/// Connect, exchange the status handshake, and build the Java [`Status`].
///
/// `handshake_host` and `handshake_port` are what the handshake packet names;
/// they are not necessarily the address the socket connects to (SRV targets
/// connect to a different host while naming the requested one).
async fn query_socket(
    socket: SocketAddr,
    handshake_host: &str,
    handshake_port: u16,
    requested: &ServerAddress,
    resolved: &ResolvedOptions,
    deadline: Instant,
) -> Result<Status, Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = time::timeout(
        crate::client::engine::remaining(deadline)?,
        TcpStream::connect(socket),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let handshake = codec::encode_status_request(handshake_host, handshake_port, -1);
    time::timeout(
        crate::client::engine::remaining(deadline)?,
        stream.write_all(&handshake),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let packet_len = time::timeout(
        crate::client::engine::remaining(deadline)?,
        codec::read_varint(&mut stream),
    )
    .await
    .map_err(|_| Error::Timeout)??;
    let packet_len = usize::try_from(packet_len)
        .map_err(|_| Error::InvalidPacket("Java status packet length is negative"))?;
    if packet_len > resolved.max_java_status_bytes {
        return Err(Error::InvalidPacket("Java status packet is too large"));
    }

    // Grow the buffer as bytes actually arrive instead of trusting the
    // declared length: a server can claim a large packet and then stall, and
    // the declaration must not turn into an up-front allocation.
    let mut packet = Vec::with_capacity(packet_len.min(PACKET_RESERVE_HINT));
    {
        let mut limited = (&mut stream).take(u64::try_from(packet_len).unwrap_or(u64::MAX));
        time::timeout(
            crate::client::engine::remaining(deadline)?,
            limited.read_to_end(&mut packet),
        )
        .await
        .map_err(|_| Error::Timeout)??;
    }
    if packet.len() != packet_len {
        return Err(Error::InvalidPacket("Java status packet is truncated"));
    }

    // Strip the packet-id and length prefix in place, then take ownership of
    // the same buffer as the raw JSON: the payload is never copied into a
    // second allocation.
    let json_offset = codec::status_response_json_offset(&packet)?;
    packet.drain(..json_offset);
    let raw_json =
        String::from_utf8(packet).map_err(|error| Error::InvalidUtf8(error.utf8_error()))?;
    // A well-formed status packet carrying garbage is a protocol failure, not
    // a blank server: without this check automatic probing would stop at a
    // broken Java response. One typed parse validates the structure and
    // yields the normalized fields; missing optional fields stay lenient.
    // A status object must be a JSON object: serde would otherwise accept
    // sequences like `[]` as field-order tuples for this struct.
    if !raw_json.trim_start().starts_with('{') {
        return Err(Error::InvalidPacket(
            "Java status JSON is not a valid status object",
        ));
    }
    let parsed = serde_json::from_str::<JavaJson>(&raw_json)
        .map_err(|_| Error::InvalidPacket("Java status JSON is not a valid status object"))?;
    Ok(Status {
        address: requested.clone(),
        motd: describe(&parsed.description),
        players: Players {
            online: parsed.players.online,
            max: parsed.players.max,
        },
        version: Version {
            name: parsed.version.name,
            protocol: parsed
                .version
                .protocol
                .and_then(|protocol| i32::try_from(protocol).ok()),
        },
        elapsed: Duration::ZERO,
        details: Details::Java(JavaDetails {
            // Shared so every singleflight waiter can clone the status
            // without copying the payload.
            raw_json: Arc::new(raw_json),
            srv: None,
        }),
    })
}

/// Typed Java status JSON used for the single structural parse.
#[derive(Deserialize)]
pub(crate) struct JavaJson {
    #[serde(default)]
    pub(crate) description: serde_json::Value,
    #[serde(default)]
    pub(crate) players: JavaPlayers,
    #[serde(default)]
    pub(crate) version: JavaVersion,
}

#[derive(Deserialize, Default)]
pub(crate) struct JavaPlayers {
    pub(crate) online: Option<u32>,
    pub(crate) max: Option<u32>,
}

#[derive(Deserialize, Default)]
pub(crate) struct JavaVersion {
    pub(crate) name: Option<String>,
    pub(crate) protocol: Option<i64>,
}

/// Flatten a Java description component into plain text.
///
/// Handles plain strings, text components (`text` plus nested `extra` arrays
/// and `translate` fallbacks), and arrays of components. Anything else is
/// rendered as its JSON form; there is no legacy color-code stripping.
fn describe(value: &serde_json::Value) -> String {
    let mut text = String::new();
    append_description(value, &mut text);
    text
}

/// Append one description component to `out`.
///
/// The whole recursive flattening shares one output buffer: array elements and
/// `extra` children are appended directly instead of each building a temporary
/// `String`.
fn append_description(value: &serde_json::Value, out: &mut String) {
    use std::fmt::Write as _;

    match value {
        serde_json::Value::String(text) => out.push_str(text),
        serde_json::Value::Array(components) => {
            for component in components {
                append_description(component, out);
            }
        }
        serde_json::Value::Object(object) => {
            let text = object
                .get("text")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty());
            match text {
                Some(text) => out.push_str(text),
                None => out.push_str(
                    object
                        .get("translate")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                ),
            }
            if let Some(extra) = object.get("extra").and_then(serde_json::Value::as_array) {
                for component in extra {
                    append_description(component, out);
                }
            }
        }
        serde_json::Value::Null => {}
        other => {
            // Numbers and booleans have no borrowed form; write them straight
            // into the output buffer.
            let _ = write!(out, "{other}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn srv_rules() {
        let resolved = ResolvedOptions {
            edition: None,
            java_port: 25_565,
            bedrock_port: 19_132,
            timeout: crate::client::DEFAULT_TIMEOUT,
            try_srv: true,
            allow_insecure_http: false,
            max_response_bytes: 64 * 1024,
            max_java_status_bytes: crate::client::DEFAULT_MAX_JAVA_STATUS_BYTES,
        };
        let mut address = crate::address::parse_server_address("play.example.com", 25_565, 19_132)
            .expect("address should parse");
        assert!(srv_applicable(&address, &resolved));

        // An explicit port pins the endpoint: no SRV.
        address = crate::address::parse_server_address("play.example.com:25566", 25_565, 19_132)
            .expect("address should parse");
        assert!(!srv_applicable(&address, &resolved));

        // IP literals never have SRV records.
        address = crate::address::parse_server_address("127.0.0.1", 25_565, 19_132)
            .expect("address should parse");
        assert!(!srv_applicable(&address, &resolved));

        // The option can disable SRV entirely.
        address = crate::address::parse_server_address("play.example.com", 25_565, 19_132)
            .expect("address should parse");
        let disabled = ResolvedOptions {
            try_srv: false,
            ..resolved.clone()
        };
        assert!(!srv_applicable(&address, &disabled));
    }

    /// Build a single-priority SRV target fixture.
    fn srv(host: &str, weight: u16) -> SrvTarget {
        SrvTarget {
            host: host.into(),
            port: 25_565,
            priority: 0,
            weight,
        }
    }

    /// Deterministic xorshift64 source for distribution tests.
    ///
    /// The production path seeds from the clock; tests pin the seed so the
    /// statistical assertions below cannot flake on CI.
    fn seeded_random(seed: u64) -> impl FnMut() -> u64 {
        let mut seed = seed | 1;
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        }
    }

    #[test]
    fn weighted_srv_order_respects_rfc_2782_weights() {
        // A heavy record dominates, but the inclusive [0, sum] draw gives the
        // zero-weight record a small chance (1/101 here) instead of none.
        let mut random = seeded_random(0x5eed_1234_5678_9abc);
        let mut heavy_first = 0;
        for _ in 0..1_000 {
            let ordered = weighted_srv_order_with(
                vec![srv("heavy.example.com", 100), srv("light.example.com", 0)],
                &mut random,
            );
            assert_eq!(ordered.len(), 2, "every record must be ordered");
            if ordered[0].host == "heavy.example.com" {
                heavy_first += 1;
            }
        }
        assert!(
            heavy_first > 900,
            "the weight-100 record should dominate, first {heavy_first}/1000 times"
        );
    }

    #[test]
    fn weighted_srv_order_gives_zero_weight_records_a_nonzero_chance() {
        // RFC 2782 places zero-weight records first and samples [0, sum]
        // inclusively, so a zero-weight record wins when the draw is zero.
        let mut random = seeded_random(0x0ddc_0ffe_e123_4567);
        let mut zero_first = 0;
        for _ in 0..2_000 {
            let ordered = weighted_srv_order_with(
                vec![
                    srv("zero.example.com", 0),
                    srv("a.example.com", 1),
                    srv("b.example.com", 1),
                ],
                &mut random,
            );
            assert_eq!(ordered.len(), 3);
            if ordered[0].host == "zero.example.com" {
                zero_first += 1;
            }
        }
        assert!(
            zero_first > 200,
            "zero-weight record was almost never selected: {zero_first}/2000"
        );
        assert!(
            zero_first < 1_200,
            "positive-weight records must still dominate: {zero_first}/2000"
        );
    }

    #[test]
    fn all_zero_weight_groups_are_selected_uniformly() {
        let mut random = seeded_random(0xabcd_ef01_2345_6789);
        let mut counts = [0_usize; 3];
        for _ in 0..3_000 {
            let ordered = weighted_srv_order_with(
                vec![
                    srv("a.example.com", 0),
                    srv("b.example.com", 0),
                    srv("c.example.com", 0),
                ],
                &mut random,
            );
            assert_eq!(ordered.len(), 3);
            let index = match ordered[0].host.as_str() {
                "a.example.com" => 0,
                "b.example.com" => 1,
                "c.example.com" => 2,
                other => panic!("unexpected host {other}"),
            };
            counts[index] += 1;
        }
        // Each record should win roughly a third of the draws (1000/3000).
        for (index, count) in counts.iter().enumerate() {
            assert!(
                *count > 300,
                "record {index} was selected only {count}/3000 times"
            );
        }
    }

    #[test]
    fn describe_flattens_nested_components() {
        let json = r#"{"description":{"text":"Hello ","extra":[{"text":"World"},{"translate":"key.menu"}]},"players":{"online":1,"max":20},"version":{"name":"1.21.4","protocol":763}}"#;
        let parsed = serde_json::from_str::<JavaJson>(json).expect("json should parse");
        let motd = describe(&parsed.description);
        let players = Players {
            online: parsed.players.online,
            max: parsed.players.max,
        };
        let version = Version {
            name: parsed.version.name,
            protocol: parsed
                .version
                .protocol
                .and_then(|protocol| i32::try_from(protocol).ok()),
        };

        assert_eq!(motd, "Hello Worldkey.menu");
        assert_eq!(players.online, Some(1));
        assert_eq!(players.max, Some(20));
        assert_eq!(version.name.as_deref(), Some("1.21.4"));
        assert_eq!(version.protocol, Some(763));
    }

    #[tokio::test]
    async fn invalid_json_fails_the_query() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client should connect");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;

            // Well-formed status frame carrying garbage JSON.
            let body = b"this is not json";
            let mut packet = vec![0x00];
            packet.push(u8::try_from(body.len()).expect("length fits in u8"));
            packet.extend_from_slice(body);
            let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
            frame.extend_from_slice(&packet);
            let _ = stream.write_all(&frame).await;
        });

        let error = query(format!("127.0.0.1:{}", address.port()))
            .await
            .expect_err("invalid JSON should fail the query");

        // Automatic probing wraps the probe failure; the inner error must be
        // the invalid-packet failure.
        let Error::NoCompatibleJavaServer { attempts, .. } = &error else {
            panic!("expected a Java aggregate error, got {error:?}");
        };
        assert!(
            matches!(
                attempts[0].error.as_ref(),
                Error::InvalidPacket("Java status JSON is not a valid status object")
            ),
            "inner failure should be the invalid JSON, got {:?}",
            attempts[0].error
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[tokio::test]
    async fn structurally_invalid_json_fails_the_query() {
        // `[]` is valid JSON but not a status object: it must fail rather
        // than produce a blank status.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client should connect");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;

            let body = b"[]";
            let mut packet = vec![0x00];
            packet.push(u8::try_from(body.len()).expect("length fits in u8"));
            packet.extend_from_slice(body);
            let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
            frame.extend_from_slice(&packet);
            let _ = stream.write_all(&frame).await;
        });

        let error = query(format!("127.0.0.1:{}", address.port()))
            .await
            .expect_err("structural mismatch should fail the query");

        let Error::NoCompatibleJavaServer { attempts, .. } = &error else {
            panic!("expected a Java aggregate error, got {error:?}");
        };
        assert!(
            matches!(
                attempts[0].error.as_ref(),
                Error::InvalidPacket("Java status JSON is not a valid status object")
            ),
            "inner failure should be the structural mismatch, got {:?}",
            attempts[0].error
        );
    }

    #[tokio::test]
    async fn explicit_port_status_keeps_the_requested_address() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client should connect");
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;

            let json = r#"{"description":"Mock","players":{"online":1,"max":20},"version":{"name":"1.21.4","protocol":763}}"#;
            let body = json.as_bytes();
            let mut packet = vec![0x00];
            packet.push(u8::try_from(body.len()).expect("length fits in u8"));
            packet.extend_from_slice(body);
            let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
            frame.extend_from_slice(&packet);
            let _ = stream.write_all(&frame).await;
        });

        let status = query(format!("127.0.0.1:{}", address.port()))
            .await
            .expect("mock java server should answer");

        // The requested address (including the explicit port) is preserved.
        assert_eq!(status.address.host, "127.0.0.1");
        assert_eq!(status.address.java_port, address.port());
        assert_eq!(status.address.explicit_port, Some(address.port()));
        assert_eq!(status.motd, "Mock");
        assert_eq!(status.players.online, Some(1));
        assert_eq!(status.version.protocol, Some(763));
    }

    #[tokio::test]
    async fn cloning_a_java_status_shares_the_raw_json() {
        let address = spawn_status_server().await;
        let status = query(format!("127.0.0.1:{}", address.port()))
            .await
            .expect("mock java server should answer");
        let clone = status.clone();

        #[allow(irrefutable_let_patterns)]
        let (Details::Java(original), Details::Java(copied)) = (&status.details, &clone.details)
        else {
            unreachable!("this build only has Java details");
        };
        assert!(
            Arc::ptr_eq(&original.raw_json, &copied.raw_json),
            "cloning a status must share the raw JSON, not copy it"
        );
        assert!(original.raw_json.as_str().contains("\"Mock\""));
    }

    #[tokio::test]
    async fn srv_timeouts_leave_time_for_the_direct_fallback() {
        // One silent SRV target and a direct address that answers. The target
        // must not consume the whole deadline: the direct fallback is part of
        // the budget split.
        let silent = spawn_silent_server().await;
        let direct = spawn_status_server().await;
        let address = crate::address::parse_server_address(
            &format!("127.0.0.1:{}", direct.port()),
            25_565,
            19_132,
        )
        .expect("address should parse");
        let targets = vec![SrvTarget {
            host: "127.0.0.1".into(),
            port: silent.port(),
            priority: 0,
            weight: 0,
        }];
        let resolved = ResolvedOptions {
            edition: None,
            java_port: address.java_port,
            bedrock_port: address.bedrock_port,
            timeout: Duration::from_millis(800),
            try_srv: true,
            allow_insecure_http: false,
            max_response_bytes: 64 * 1024,
            max_java_status_bytes: crate::client::DEFAULT_MAX_JAVA_STATUS_BYTES,
        };
        let deadline = Instant::now() + Duration::from_millis(800);

        let status = probe_srv_sequence(&address, &targets, &resolved, deadline)
            .await
            .expect("the direct fallback should still answer");

        assert_eq!(status.motd, "Mock");
        assert!(
            matches!(&status.details, Details::Java(details) if details.srv.is_none()),
            "the answer must come from the direct fallback"
        );
    }

    #[tokio::test]
    async fn srv_handshake_names_the_requested_hostname() {
        // The connection goes to the SRV target, but the handshake must name
        // the host the caller asked for: virtual-host proxies route on it.
        let (target, captured) = spawn_handshake_capture_mock().await;
        let requested =
            crate::address::parse_server_address("play.example.com:25565", 25_565, 19_132)
                .expect("address should parse");
        let targets = vec![SrvTarget {
            host: "127.0.0.1".into(),
            port: target.port(),
            priority: 0,
            weight: 0,
        }];
        let resolved = ResolvedOptions {
            edition: None,
            java_port: requested.java_port,
            bedrock_port: requested.bedrock_port,
            timeout: Duration::from_secs(2),
            try_srv: true,
            allow_insecure_http: false,
            max_response_bytes: 64 * 1024,
            max_java_status_bytes: crate::client::DEFAULT_MAX_JAVA_STATUS_BYTES,
        };
        let deadline = Instant::now() + Duration::from_secs(2);

        let status = probe_srv_sequence(&requested, &targets, &resolved, deadline)
            .await
            .expect("the SRV target should answer");
        assert_eq!(status.motd, "Mock");

        let request = captured.lock().expect("capture lock poisoned").clone();
        let text = String::from_utf8_lossy(&request);
        assert!(
            text.contains("play.example.com"),
            "the handshake must name the requested host: {text:?}"
        );
        assert!(
            !text.contains("127.0.0.1"),
            "the handshake must not name the SRV target: {text:?}"
        );
    }

    #[test]
    fn srv_ordering_varies_between_calls() {
        // The production seed must not repeat across calls in the same clock
        // tick: 32 draws of a four-record all-zero group should not all agree.
        let mut firsts = std::collections::HashSet::new();
        for _ in 0..32 {
            let ordered = weighted_srv_order(vec![
                srv("a.example.com", 0),
                srv("b.example.com", 0),
                srv("c.example.com", 0),
                srv("d.example.com", 0),
            ]);
            firsts.insert(ordered[0].host.clone());
        }
        assert!(
            firsts.len() > 1,
            "the production SRV seed repeated across calls: {firsts:?}"
        );
    }

    /// Spawn a Java status server that records the first request bytes and
    /// answers with a valid status payload.
    async fn spawn_handshake_capture_mock() -> (std::net::SocketAddr, Arc<Mutex<Vec<u8>>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 512];
            let len = stream.read(&mut request).await.unwrap_or(0);
            *sink.lock().expect("capture lock poisoned") = request[..len].to_vec();

            let json = r#"{"description":"Mock","players":{"online":1,"max":20},"version":{"name":"1.21.4","protocol":763}}"#;
            let body = json.as_bytes();
            let mut packet = vec![0x00];
            packet.push(u8::try_from(body.len()).expect("length fits in u8"));
            packet.extend_from_slice(body);
            let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
            frame.extend_from_slice(&packet);
            let _ = stream.write_all(&frame).await;
        });
        (address, captured)
    }

    /// Spawn a Java status server that answers every connection with a valid
    /// status payload.
    async fn spawn_status_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut request = [0_u8; 256];
                    let _ = stream.read(&mut request).await;

                    let json = r#"{"description":"Mock","players":{"online":1,"max":20},"version":{"name":"1.21.4","protocol":763}}"#;
                    let body = json.as_bytes();
                    let mut packet = vec![0x00];
                    packet.push(u8::try_from(body.len()).expect("length fits in u8"));
                    packet.extend_from_slice(body);
                    let mut frame = vec![u8::try_from(packet.len()).expect("length fits in u8")];
                    frame.extend_from_slice(&packet);
                    let _ = stream.write_all(&frame).await;
                });
            }
        });
        address
    }

    /// Spawn a TCP server that accepts one connection and never answers, so
    /// the client's status read runs into its deadline.
    async fn spawn_silent_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("silent listener should bind");
        let address = listener.local_addr().expect("mock address should exist");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        address
    }
}
