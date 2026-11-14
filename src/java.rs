//! Java Edition status queries.
//!
//! [`query`] resolves `_minecraft._tcp` SRV records when the address has no
//! explicit port, then probes the Java status endpoint over TCP. For custom
//! timeouts and options use [`query_with`] or [`crate::Client`]. The pure
//! protocol codec lives in [`crate::protocol::java`].

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time;

use crate::address::{self, ServerAddress};
use crate::client::QueryOptions;
use crate::error::{Attempt, Error, Result};
use crate::protocol::java as codec;
use crate::status::{Details, JavaDetails, Players, Status, Transport, Version};

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

/// Raw Java Edition status payload kept for advanced callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JavaQueryInfo {
    /// Raw JSON returned by the server status endpoint.
    pub raw_json: String,
    /// SRV record the query was resolved through, if any.
    pub srv: Option<SrvTarget>,
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
    crate::client::default_client().java(address).await
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
    crate::client::default_client()
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
    let host = address::normalize_server_host(host.as_ref())?;
    if !address::can_query_srv(host.as_ref()) {
        return Ok(Vec::new());
    }

    let resolver = hickory_resolver::Resolver::builder_tokio()
        .map_err(|error| Error::Dns(error.to_string()))?
        .build();
    let name = address::minecraft_srv_name(host.as_ref());
    let lookup = time::timeout(timeout, resolver.srv_lookup(name))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|error| Error::Dns(error.to_string()))?;

    let records = lookup
        .iter()
        .map(|srv| SrvTarget {
            host: srv.target().to_utf8().trim_end_matches('.').to_string(),
            port: srv.port(),
            priority: srv.priority(),
            weight: srv.weight(),
        })
        .filter(|target| !target.host.is_empty() && target.port != 0)
        .collect::<Vec<_>>();
    Ok(weighted_srv_order(records))
}

/// Order SRV records by priority with RFC 2782 weighted-random selection
/// inside each priority class.
///
/// Zero-weight records receive a minimal ticket so they remain reachable,
/// deviating slightly from the strict RFC reading.
fn weighted_srv_order(mut records: Vec<SrvTarget>) -> Vec<SrvTarget> {
    records.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.host.cmp(&right.host))
            .then_with(|| left.port.cmp(&right.port))
    });

    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1_u64, |duration| u64::from(duration.subsec_nanos()) | 1);
    let mut random = move || {
        // xorshift64: tiny dependency-free PRNG, good enough for ordering.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    let mut ordered = Vec::with_capacity(records.len());
    while !records.is_empty() {
        let priority = records[0].priority;
        let group_end = records
            .iter()
            .take_while(|target| target.priority == priority)
            .count();
        let mut group: Vec<SrvTarget> = records.drain(0..group_end).collect();
        while !group.is_empty() {
            let total: u64 = group
                .iter()
                .map(|target| u64::from(target.weight) + 1)
                .sum();
            let mut point = random() % total;
            let mut picked = 0;
            for (position, target) in group.iter().enumerate() {
                let tickets = u64::from(target.weight) + 1;
                if point < tickets {
                    picked = position;
                    break;
                }
                point -= tickets;
            }
            ordered.push(group.remove(picked));
        }
    }
    ordered
}

/// Full Java path used by the engine: SRV records first, then direct.
pub(crate) async fn run(
    address: &ServerAddress,
    options: &QueryOptions,
    deadline: Instant,
) -> Result<Status> {
    let mut prefix_attempts: Vec<Attempt> = Vec::new();

    if options.try_srv && address::can_query_srv(&address.host) {
        // Cap the SRV budget so a slow SRV path cannot starve direct probes.
        let srv_budget =
            crate::client::engine::remaining(deadline)?.min(crate::client::SRV_LOOKUP_BUDGET);
        match resolve_srv(&address.host, srv_budget).await {
            Ok(targets) if !targets.is_empty() => {
                return probe_srv_sequence(address, &targets, deadline)
                    .await
                    .map_err(|attempts| Error::NoCompatibleJavaServer {
                        host: address.host.clone(),
                        java_port: address.java_port,
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

    let ips = crate::client::engine::resolve_probe_ips(&address.host, deadline)
        .await
        .map_err(|error| Error::NoCompatibleJavaServer {
            host: address.host.clone(),
            java_port: address.java_port,
            attempts: vec![Attempt {
                transport: Transport::JavaTcp,
                target: address.host.clone(),
                error: Box::new(error),
            }],
        })?;

    probe_direct(address, &ips, deadline)
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
    deadline: Instant,
) -> Result<Status, Vec<Attempt>> {
    let mut attempts = Vec::new();
    for target in targets {
        match probe_target(target, deadline).await {
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
    match probe_direct(address, &[], deadline).await {
        Ok(status) => Ok(status),
        Err(mut direct_attempts) => {
            attempts.append(&mut direct_attempts);
            Err(attempts)
        }
    }
}

/// Probe one SRV target; the target host is resolved internally.
async fn probe_target(target: &SrvTarget, deadline: Instant) -> Result<Status, Error> {
    let ips = crate::client::engine::resolve_probe_ips(&target.host, deadline).await?;
    let socket = SocketAddr::new(ips[0], target.port);
    let mut status = query_socket(socket, &target.host, target.port, 0, deadline).await?;
    #[allow(irrefutable_let_patterns)]
    if let Status {
        details: Details::Java(details),
        ..
    } = &mut status
    {
        details.srv = Some(target.clone());
    }
    Ok(status)
}

/// Probe the direct Java port, trying every resolved address in order until
/// one answers. With an empty `ips` slice the input host is resolved first.
/// The whole sequence is deadline-bounded end to end.
pub(crate) async fn probe_direct(
    address: &ServerAddress,
    ips: &[IpAddr],
    deadline: Instant,
) -> Result<Status, Vec<Attempt>> {
    let mut attempts = Vec::new();
    let resolved;
    let candidate_ips: &[IpAddr] = if ips.is_empty() {
        resolved = crate::client::engine::resolve_probe_ips(&address.host, deadline)
            .await
            .map_err(|error| {
                vec![Attempt {
                    transport: Transport::JavaTcp,
                    target: address.host.clone(),
                    error: Box::new(error),
                }]
            })?;
        &resolved
    } else {
        ips
    };

    for ip in candidate_ips {
        let socket = SocketAddr::new(*ip, address.java_port);
        match query_socket(
            socket,
            &address.host,
            address.java_port,
            address.bedrock_port,
            deadline,
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
async fn query_socket(
    socket: SocketAddr,
    host: &str,
    port: u16,
    bedrock_port: u16,
    deadline: Instant,
) -> Result<Status, Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = time::timeout(
        crate::client::engine::remaining(deadline)?,
        TcpStream::connect(socket),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let handshake = codec::encode_status_request(host, port, -1);
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
    if packet_len > codec::MAX_STATUS_PACKET_LEN {
        return Err(Error::InvalidPacket("Java status packet is too large"));
    }

    let mut packet = vec![0_u8; packet_len];
    time::timeout(
        crate::client::engine::remaining(deadline)?,
        stream.read_exact(&mut packet),
    )
    .await
    .map_err(|_| Error::Timeout)??;

    let raw_json = codec::parse_status_response(&packet)?.to_string();
    let address = ServerAddress {
        host: host.to_string(),
        java_port: port,
        bedrock_port,
        explicit_port: None,
    };
    Ok(Status::from_details(
        address,
        Details::Java(JavaDetails {
            raw_json,
            srv: None,
        }),
    ))
}

/// Extract the normalized MOTD text, player counts, and version from the raw
/// Java status JSON. Unparseable JSON yields empty defaults; the raw payload
/// stays available in [`JavaDetails`].
pub(crate) fn extract_java_fields(raw_json: &str) -> (String, Players, Version) {
    #[derive(Deserialize)]
    struct JavaJson {
        #[serde(default)]
        description: serde_json::Value,
        #[serde(default)]
        players: JavaPlayers,
        #[serde(default)]
        version: JavaVersion,
    }
    #[derive(Deserialize, Default)]
    struct JavaPlayers {
        online: Option<u32>,
        max: Option<u32>,
    }
    #[derive(Deserialize, Default)]
    struct JavaVersion {
        name: Option<String>,
        protocol: Option<i64>,
    }

    let Ok(parsed) = serde_json::from_str::<JavaJson>(raw_json) else {
        return (String::new(), Players::default(), Version::default());
    };

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
    (motd, players, version)
}

/// Flatten a Java description component (string or `{"text": ...}` object)
/// into plain text.
fn describe(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(object) => object
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| value.to_string(), ToString::to_string),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}
